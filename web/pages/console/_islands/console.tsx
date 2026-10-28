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
  apiPost,
  type ActionsPage,
  type AccountSummary,
  type AdminInvitesPage,
  type ApiKeysSummary,
  type AppDetail,
  type CollaboratorsSummary,
  type ConnectionsSummary,
  type ConsoleApp,
  type DomainsSummary,
  type ErrorsSummary,
  type InvitesSummary,
  type LogLine,
  type MetricsSummary,
  type SecretsSummary,
  type WaitlistSummary,
  when,
} from "./api.ts";
import type { AdminStats, ModerationQueue } from "./actions/admin-moderation.ts";
import { fetchTelemetry, type TelemetrySummary } from "./views/telemetry.tsx";
import {
  AccountIcon,
  ActionsIcon,
  CloseIcon,
  CodeIcon,
  CollapseIcon,
  DatabaseIcon,
  DeploysIcon,
  DocsIcon,
  ErrorsIcon,
  ExpandIcon,
  LogsIcon,
  MenuIcon,
  MetricsIcon,
  OverviewIcon,
  PlusIcon,
  SearchIcon,
  SecretsIcon,
  SettingsIcon,
  ShieldIcon,
  SignOutIcon,
  TicketIcon,
} from "./icons.tsx";
import {
  ActionsView,
  AdminView,
  DeploysView,
  EmptyState,
  InvitesView,
  MetricsView,
  OverviewView,
  SecretsView,
  SettingsView,
} from "./views.tsx";
import { CodeView } from "./views/code.tsx";
import { D1Nav, D1View } from "./views/d1.tsx";

import type { ConsoleBridge } from "./actions/settings.ts";
import { domainActions } from "./actions/domains.ts";
import { Palette } from "./palette.tsx";
import {
  createConfirmHandler,
  syncWebMcp,
  type ConfirmEntry,
  type ExecuteContext,
} from "./webmcp.ts";
import { secretsActions } from "./actions/secrets.ts";
import { ErrorsView } from "./views/errors.tsx";
import { LogsView } from "./views/logs.tsx";
import {
  AccountView,
  ConnectionsView,
  UsageView,
} from "./views/account.tsx";

import {
  ACCOUNT_TABS,
  CopyButton,
  DATA_VIEWS,
  RANGES,
  VIEWS,
  canOpenApp,
  hrefFor,
  linkTo,
  mergeRoute,
  readRoute,
  routeUrl,
  type ActionsPanel,
  type Panel,
  type Route,
  type ViewId,
} from "./common.tsx";
import { DoView } from "./data/do/view.tsx";
import { R2View } from "./data/r2/view.tsx";

// --- Shared bits -------------------------------------------------------------

function monogram(slug: string): string {
  const clean = slug.replace(/[^a-z0-9]/gi, "");
  return (clean.slice(0, 1) || "?").toUpperCase();
}

// daisyUI mappings: status pills are `badge badge-soft`, rail dots are the
// `status` component inside `indicator-item` (pinned inside the tile bounds
// by `.rail-dot` in console.css, so the rail never overflows).
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
  if (status === "expired") {
    return "badge-warning";
  }
  return "badge-ghost";
}

function statusDot(status: string): string {
  if (status === "success" || status === "running" || status === "ok") {
    return "status-success";
  }
  if (
    status === "building" ||
    status === "deploying" ||
    status === "reviewing" ||
    status === "queued"
  ) {
    return "status-warning";
  }
  if (status === "failed" || status === "error" || status === "blocked") {
    return "status-error";
  }
  if (status === "expired") {
    return "status-warning";
  }
  return "status-neutral";
}

// Rail tooltip for the apps list: status, sharing, and the inactivity
// deadline (spec §7.8) when one applies.
function appTip(a: ConsoleApp, sep: string): string {
  const shared = a.role === "collaborator" ? `${sep}Shared` : "";
  const deletes = a.deletes_at != null ? `${sep}deletes ${when(a.deletes_at)}` : "";
  return `${a.slug}${sep}${a.status}${shared}${deletes}`;
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

const COLLAPSED_KEY = "outer-console-collapsed";

function readCollapsed(): boolean {
  if (typeof window === "undefined") {
    return false;
  }
  return window.localStorage.getItem(COLLAPSED_KEY) === "1";
}


const VIEW_TITLES: Record<ViewId, string> = {
  overview: "Overview",
  metrics: "Metrics",
  actions: "Agent actions",
  deploys: "Deploys",
  code: "Code",
  logs: "Logs",
  errors: "Errors",
  secrets: "Secrets",
  settings: "Settings",
  d1: "D1",
  invites: "Invite codes",
  admin: "Admin",
  connections: "Connections & keys",
  usage: "Usage & plan",
  account: "Account",
  r2: "R2",
  do: "Durable Objects",
};

/** Shell UI state: sidebar collapse, mobile drawer, popovers, expanded deploy rows. */
interface Ui {
  collapsed: boolean;
  drawer: boolean;
  createOpen: boolean;
  accountOpen: boolean;
  expanded: string[];
  paletteOpen: boolean;
}

/** The modal names the target, not just the action: first concrete input wins. */
function confirmTargetOf(input: Record<string, unknown>): string {
  const keys = ["slug", "app", "email", "name", "prefix", "id", "fingerprint", "sha", "bucket", "table"];
  for (const key of keys) {
    const value = input[key];
    if (typeof value === "string" && value !== "") {
      return value;
    }
  }
  return "the current app";
}

export function Console() {
  const route = atom<Route>(readRoute());
  const apps = atom<Panel<ConsoleApp[]>>({ kind: "loading" });
  const email = atom("");
  const plan = atom("free");
  const isAdmin = atom(false);
  const invites = atom<Panel<InvitesSummary>>({ kind: "loading" });
  const adminInvites = atom<Panel<AdminInvitesPage>>({ kind: "loading" });
  const waitlist = atom<Panel<WaitlistSummary>>({ kind: "loading" });
  const moderationQueue = atom<Panel<ModerationQueue>>({ kind: "loading" });
  const adminStats = atom<Panel<AdminStats>>({ kind: "loading" });
  const telemetry = atom<Panel<TelemetrySummary>>({ kind: "loading" });
  const detail = atom<Panel<AppDetail>>({ kind: "loading" });
  const metrics = atom<Panel<{ hours: number; summary: MetricsSummary }>>({
    kind: "loading",
  });
  const actions = atom<ActionsPanel>({ kind: "loading" });
  const errors = atom<Panel<ErrorsSummary>>({ kind: "loading" });
  const logs = atom<Panel<LogLine[]>>({ kind: "loading" });
  const secrets = atom<Panel<SecretsSummary>>({ kind: "loading" });
  const connections = atom<Panel<ConnectionsSummary>>({ kind: "loading" });
  const apiKeys = atom<Panel<ApiKeysSummary>>({ kind: "loading" });
  const accountSummary = atom<Panel<AccountSummary>>({ kind: "loading" });
  const collaborators = atom<Panel<CollaboratorsSummary>>({ kind: "loading" });
  const domains = atom<Panel<DomainsSummary>>({ kind: "loading" });
  const ui = atom<Ui>({
    collapsed: readCollapsed(),
    drawer: false,
    createOpen: false,
    accountOpen: false,
    expanded: [],
    paletteOpen: false,
  });
  const pendingConfirm = atom<ConfirmEntry | null>(null);
  // A fingerprint picked in Logs ("View issue"): the Errors view consumes
  // it on mount and expands it. Owned here, passed down as props.
  const pendingJump = atom<string | null>(null);
  const bridge: ConsoleBridge = {
    getEmail: () => email(),
    getRoute: () => route(),
    getApps: () => {
      const panel = apps();
      return panel.kind === "ready" ? panel.value : [];
    },
    navigate,
  };
  const webmcpCtx: ExecuteContext = {
    getSlug: () => route().app,
    bridge,
    isAdmin: () => isAdmin(),
    confirm: createConfirmHandler(
      () => pendingConfirm(),
      (next) => pendingConfirm.set(next),
    ),
  };

  // One generation per navigation: late responses from the previous app
  // never repaint the new one (Noite keys its cards per app id instead).
  let epoch = 0;

  // The hub tab label, used in the tab title and both hub headings.
  function hubTitle(r: Route): string {
    return ACCOUNT_TABS.find((t) => t.id === r.acctab)?.label ?? "Account";
  }

  function setTitle(r: Route, app: AppDetail | null): void {
    document.title =
      app && r.app
        ? `${r.app} · ${VIEW_TITLES[r.view]} · Outer console`
        : r.view === "account"
          ? `${hubTitle(r)} · Account · Outer console`
          : isAccountView(r.view)
            ? `${VIEW_TITLES[r.view]} · Outer console`
            : "Console · Outer";
  }

  function loadersFor(view: ViewId): Array<"metrics" | "actions" | "errors" | "logs" | "secrets" | "collaborators" | "domains"> {
    switch (view) {
      case "overview":
        return ["metrics", "actions", "errors"];
      case "metrics":
        return ["metrics"];
      case "actions":
        return ["actions"];
      case "deploys":
      case "code":
      case "d1":
        return [];
      case "logs":
        return ["logs"];
      case "errors":
        return ["errors"];
      case "secrets":
        return ["secrets"];
      case "settings":
        return ["collaborators", "domains"];
      case "account":
      case "admin":
      case "r2":
      case "do":
        return [];
      default: {
        // Legacy `?t=usage|connections|invites` routes never reach the
        // loaders (readRoute maps them onto the hub), but exhaustiveness
        // here keeps the next added view honest.
        const _exhaustive: "connections" | "usage" | "invites" = view;
        return [];
      }
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
        secretsActions.listSecrets
          .run({ slug })
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
    if (loadersFor(view).includes("collaborators")) {
      if (!quiet) {
        collaborators.set({ kind: "loading" });
      }
      jobs.push(
        api<CollaboratorsSummary>(
          `/api/console/apps/${encodeURIComponent(slug)}/collaborators`,
        )
          .then((summary) => {
            put(collaborators, { kind: "ready", value: summary });
          })
          .catch((err: unknown) => {
            if (!quiet) {
              put(collaborators, { kind: "error", message: String(err) });
            }
          }),
      );
    }
    if (loadersFor(view).includes("domains")) {
      if (!quiet) {
        domains.set({ kind: "loading" });
      }
      jobs.push(
        domainActions.listDomains
          .run({ app: slug })
          .then((summary) => {
            put(domains, { kind: "ready", value: summary });
          })
          .catch((err: unknown) => {
            if (!quiet) {
              put(domains, { kind: "error", message: String(err) });
            }
          }),
      );
    }
    await Promise.all(jobs);
  }

  function isAccountView(view: ViewId): boolean {
    return view === "account" || view === "admin";
  }

  async function loadAccountView(r: Route, myEpoch: number): Promise<void> {
    const fresh = () => epoch === myEpoch;
    if (r.view === "admin") {
      adminInvites.set({ kind: "loading" });
      waitlist.set({ kind: "loading" });
      moderationQueue.set({ kind: "loading" });
      adminStats.set({ kind: "loading" });
      telemetry.set({ kind: "loading" });
      // Each panel stands alone: a failing moderation endpoint must not
      // wipe the invite-code or waitlist panels (or vice versa).
      const [page, list, queue, stats, calls] = await Promise.allSettled([
        api<AdminInvitesPage>("/api/console/admin/invites?limit=50"),
        api<WaitlistSummary>("/api/console/admin/waitlist?limit=100"),
        api<ModerationQueue>("/api/console/admin/moderation/queue"),
        api<AdminStats>("/api/console/admin/stats"),
        fetchTelemetry(),
      ]);
      if (fresh()) {
        const settle = <T,>(res: PromiseSettledResult<T>): Panel<T> =>
          res.status === "fulfilled"
            ? { kind: "ready", value: res.value }
            : { kind: "error", message: String(res.reason) };
        adminInvites.set(settle(page));
        waitlist.set(settle(list));
        moderationQueue.set(settle(queue));
        adminStats.set(settle(stats));
        telemetry.set(settle(calls));
        setTitle(route(), null);
      }
      return;
    }
    if (r.acctab === "invites") {
      invites.set({ kind: "loading" });
      try {
        const summary = await api<InvitesSummary>("/api/console/invites");
        if (fresh()) {
          invites.set({ kind: "ready", value: summary });
          setTitle(route(), null);
        }
      } catch (err: unknown) {
        if (fresh()) {
          invites.set({ kind: "error", message: String(err) });
        }
      }
      return;
    }
    if (r.acctab === "connections") {
      connections.set({ kind: "loading" });
      apiKeys.set({ kind: "loading" });
      try {
        const [conns, keys] = await Promise.all([
          api<ConnectionsSummary>("/api/console/connections"),
          api<ApiKeysSummary>("/api/console/api-keys"),
        ]);
        if (fresh()) {
          connections.set({ kind: "ready", value: conns });
          apiKeys.set({ kind: "ready", value: keys });
          setTitle(route(), null);
        }
      } catch (err: unknown) {
        if (fresh()) {
          const message = String(err);
          connections.set({ kind: "error", message });
          apiKeys.set({ kind: "error", message });
        }
      }
      return;
    }
    // The Account and Usage tabs share the summary endpoint.
    accountSummary.set({ kind: "loading" });
    try {
      const summary = await api<AccountSummary>("/api/console/account");
      if (fresh()) {
        accountSummary.set({ kind: "ready", value: summary });
        setTitle(route(), null);
      }
    } catch (err: unknown) {
      if (fresh()) {
        accountSummary.set({ kind: "error", message: String(err) });
      }
    }
  }

  async function generateCodes(count: number, note: string): Promise<void> {
    await apiPost<{ codes: string[] }>("/api/console/admin/invites", {
      count,
      note: note || undefined,
    });
    await loadAccountView(route(), epoch);
  }

  async function revokeCode(code: string): Promise<void> {
    await apiPost<{ revoked: boolean }>(
      `/api/console/admin/invites/${encodeURIComponent(code)}/revoke`,
      {},
    );
    await loadAccountView(route(), epoch);
  }

  async function loadMoreAdminCodes(): Promise<void> {
    const current = adminInvites();
    if (current.kind !== "ready" || !current.value.next) {
      return;
    }
    const before = current.value.next;
    try {
      const page = await api<AdminInvitesPage>(
        `/api/console/admin/invites?limit=50&before=${encodeURIComponent(before)}`,
      );
      const latest = adminInvites();
      if (latest.kind !== "ready") {
        return;
      }
      adminInvites.set({
        kind: "ready",
        value: { codes: [...latest.value.codes, ...page.codes], next: page.next },
      });
    } catch {
      // The table keeps what it has; retry via the view's Retry button.
    }
  }

  async function enter(r: Route, myEpoch: number): Promise<void> {
    scrollRailActive();
    if (isAccountView(r.view)) {
      setTitle(r, null);
      await loadAccountView(r, myEpoch);
      if (!r.app) {
        return;
      }
      // Fall through: keep the app detail (and the sidebar) live too.
    } else if (!r.app) {
      return;
    }
    const fresh = () => epoch === myEpoch;
    detail.set({ kind: "loading" });
    metrics.set({ kind: "loading" });
    actions.set({ kind: "loading" });
    errors.set({ kind: "loading" });
    logs.set({ kind: "loading" });
    secrets.set({ kind: "loading" });
    collaborators.set({ kind: "loading" });
    domains.set({ kind: "loading" });
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
    // Agents and old callers may still navigate by legacy top-level
    // account view (`usage`, …): mergeRoute lands them on the matching
    // hub tab, and anchors share the same merge through hrefFor.
    const r = mergeRoute(prev, next);
    const url = routeUrl(r);
    if (replace) {
      window.history.replaceState(null, "", url);
    } else {
      window.history.pushState(null, "", url);
    }
    route.set(r);
    const current = ui();
    ui.set({ ...current, drawer: false, createOpen: false });
    scrollRailActive();
    epoch += 1;
    if (r.app !== prev.app) {
      void enter(r, epoch);
      return;
    }
    if (isAccountView(r.view)) {
      setTitle(r, null);
      void loadAccountView(r, epoch);
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

  // patchUi lives above `watch.once`: the watcher's first pass runs
  // while the boot skeleton early-returns below, and its keydown listener
  // closes over this scope — a later definition would stay unassigned.
  const patchUi = (patch: Partial<Ui>) => {
    if (patch.collapsed !== undefined && typeof window !== "undefined") {
      window.localStorage.setItem(COLLAPSED_KEY, patch.collapsed ? "1" : "0");
    }
    ui.set({ ...ui(), ...patch });
  };

  // Phones show the app picker as a horizontal snap row: keep the
  // current app's tile in view after boot and every navigation.
  function scrollRailActive(): void {
    if (typeof window === "undefined") {
      return;
    }
    window.requestAnimationFrame(() => {
      document
        .querySelector(".rail-apps .rail-tile.is-active")
        ?.scrollIntoView({ block: "nearest", inline: "center" });
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
          const me = await api<{ email: string; plan?: string; is_admin?: boolean }>(
            "/api/console/me",
          );
          email.set(me.email);
          plan.set(me.plan ?? "free");
          isAdmin.set(me.is_admin ?? false);
          syncWebMcp(me.email || null, webmcpCtx);
        } catch {
          email.set("");
          plan.set("free");
          isAdmin.set(false);
          syncWebMcp(null, webmcpCtx);
        }
        const r = route();
        if (isAccountView(r.view)) {
          // Account views never depend on the app: a missing or foreign
          // `?app=` still shows them instead of hanging on skeletons.
          epoch += 1;
          await enter(r, epoch);
          return;
        }
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
        if (!canOpenApp(isAdmin(), data.apps.map((a) => a.slug), r.app)) {
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
        const r = route();
        if (isAccountView(r.view)) {
          // Account views never need the app list: a failing
          // `/api/console/apps` still shows them instead of the fatal
          // error screen.
          epoch += 1;
          void enter(r, epoch);
          return;
        }
        detail.set({ kind: "error", message: String(err) });
      });
    // ⌘K + WebMCP wiring: `webmcpCtx` already carries the bridge for the
    // navigation actions and the daisyUI confirm gate; here only the
    // identity-keyed registration stays (re-register on identity change,
    // abort on unload).
    syncWebMcp(email() || null, webmcpCtx);
    window.addEventListener("pagehide", () => {
      syncWebMcp(null, webmcpCtx);
    });
    window.addEventListener("keydown", (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        patchUi({ paletteOpen: !ui().paletteOpen });
        return;
      }
      if (e.key === "Escape") {
        const current = ui();
        if (current.paletteOpen) {
          ui.set({ ...current, paletteOpen: false });
        } else if (current.createOpen || current.accountOpen || current.drawer) {
          ui.set({ ...current, createOpen: false, accountOpen: false, drawer: false });
        }
      }
    });
    window.addEventListener("popstate", () => {
      const r = readRoute();
      route.set(r);
      if (isAccountView(r.view)) {
        // Like boot: the account view stands alone, whatever `?app=` says.
        epoch += 1;
        void enter(r, epoch);
        return;
      }
      if (!r.app) {
        return;
      }
      const known = apps();
      if (
        known.kind === "ready" && r.app &&
        !canOpenApp(isAdmin(), known.value.map((a) => a.slug), r.app)
      ) {
        detail.set({ kind: "error", message: "request failed: 404" });
        return;
      }
      epoch += 1;
      void enter(r, epoch);
    });
    // Noite's live feed, polling edition: refresh the visible view only.
    // Account views never poll: invites change through this shell alone.
    window.setInterval(() => {
      const r = route();
      if (!r.app || isAccountView(r.view) || document.hidden) {
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
  // A failing `/api/console/apps` is fatal for app views only: account
  // views (`?t=connections`, `?t=usage`, ...) stand alone on an empty rail.
  const list = appsPanel.kind === "ready" ? appsPanel.value : [];
  const accountView = isAccountView(r.view);
  if (appsPanel.kind === "error" && !accountView) {
    return (
      <div class="console-error" role="alert">
        <div class="alert alert-error alert-soft max-w-md">
          <span class="min-w-0">Couldn't load your apps: {appsPanel.message}</span>
        </div>
        <a class="btn btn-neutral" href="/console/sign-in">Sign in</a>
      </div>
    );
  }

  // admin-all-apps branch: a pure admin owns no apps, so the rail is empty;
  // when they open an app from the directory, the loaded detail (not the
  // rail) decides — otherwise every foreign app renders "No apps yet".
  if (list.length === 0 && !accountView && detailPanel.kind !== "ready") {
    return (
      <div class="console-empty" role="status">
        <img src="/logo.svg" alt="Outer home" width={28} height={28} />
        <h1>No apps yet</h1>
        <p>
          Apps are created by your agent over MCP at{" "}
          <code>https://outer.now/mcp</code> — there is nothing to click here
          yet.
        </p>
      </div>
    );
  }
  function reloadAccount(): void {
    epoch += 1;
    void loadAccountView(route(), epoch);
  }
  const hubPanel = (
    r.acctab === "connections" ? (
      <ConnectionsView
        connections={connections()}
        keys={apiKeys()}
        onReload={reloadAccount}
        onRetry={reloadAccount}
      />
    ) : r.acctab === "usage" ? (
      <UsageView panel={accountSummary()} onRetry={reloadAccount} />
    ) : r.acctab === "invites" ? (
      <InvitesView
        panel={invites()}
        isAdmin={isAdmin()}
        onRetry={reloadAccount}
      />
    ) : (
      <AccountView panel={accountSummary()} onRetry={reloadAccount} />
    )
  );
  const adminPanel = (
    <AdminView
      codes={adminInvites()}
      waitlist={waitlist()}
      queue={moderationQueue()}
      stats={adminStats()}
      telemetry={telemetry()}
      isAdmin={isAdmin()}
      onGenerate={(count, note) => generateCodes(count, note)}
      onRevoke={(code) => revokeCode(code)}
      onMoreCodes={() => void loadMoreAdminCodes()}
      onRetry={reloadAccount}
      onMutated={reloadAccount}
      onOpenCode={(slug, sha) => navigate({ app: slug, view: "code", sha })}
      onOpenApp={(slug) => navigate({ app: slug, view: "overview" })}
    />
  );
  const accountPanel = accountView ? (r.view === "admin" ? adminPanel : hubPanel) : null;
  const active =
    list.find((a) => a.slug === r.app) ??
    (detailPanel.kind === "ready" ? detailPanel.value : null);
  const errorsPanel = errors();
  const openErrors =
    errorsPanel.kind === "ready"
      ? errorsPanel.value.issues.filter((i) => i.status === "open").length
      : 0;

  // (patchUi moved above `watch.once`; see the note there.)

  // Reload the visible view's data (error Retry buttons reuse it).
  function reload(): void {
    const current = route();
    if (current.app && !isAccountView(current.view)) {
      void loadViewData(current.app, current.view, current.range, epoch, false);
    } else if (isAccountView(current.view)) {
      epoch += 1;
      void loadAccountView(current, epoch);
    }
  }

  // rollback change what the detail carries, rename changes the slug, and
  // delete changes the rail.
  function refreshDetail(): void {
    const r = route();
    if (r.app && !isAccountView(r.view)) {
      epoch += 1;
      void enter(r, epoch);
    }
  }

  async function reloadApps(): Promise<ConsoleApp[]> {
    try {
      const data = await api<{ apps: ConsoleApp[] }>("/api/console/apps");
      apps.set({ kind: "ready", value: data.apps });
      return data.apps;
    } catch {
      return [];
    }
  }

  // After a rename: refresh the rail, then move to the new slug.
  function afterRename(newSlug: string): void {
    void reloadApps().then(() => {
      navigate({ app: newSlug, view: "settings" });
    });
  }

  // After a delete: refresh the rail, then move to the next app (or clear
  // the app, which renders the no-apps state).
  function afterDelete(): void {
    const gone = route().app;
    void reloadApps().then((list) => {
      const next = list.find((a) => a.slug !== gone) ?? null;
      navigate({ app: next?.slug ?? null, view: "overview" });
    });
  }


  return (
    <div class="console">
      <a class="skip-link" href="#console-main">
        Skip to content
      </a>
      <nav class="rail" aria-label="Applications">
        <a
          class="rail-tile rail-logo tooltip tooltip-right"
          data-tip="Outer console"
          href={list.length > 0 ? hrefFor(r, { app: list[0].slug, view: "overview" }) : "/console"}
          aria-label="Outer console"
          onclick={(e: Event) => {
            if (list.length > 0) {
              linkTo(e, () => navigate({ app: list[0].slug, view: "overview" }));
            }
          }}
        >
          <img src="/logo.svg" alt="" width={22} height={22} />
        </a>
        <div class="rail-apps" role="list">
          {list.map((a) => (
            <a
              key={a.slug}
              role="listitem"
              href={hrefFor(r, { app: a.slug, view: "overview" })}
              class={`rail-tile tooltip tooltip-right${a.slug === r.app ? " is-active" : ""}`}
              data-tip={appTip(a, " · ")}
              title={appTip(a, " · ")}
              aria-label={a.role === "collaborator" ? `${a.slug}, ${a.status}, shared with you${a.deletes_at != null ? `, deletes ${when(a.deletes_at)}` : ""}` : `${a.slug}, ${a.status}${a.deletes_at != null ? `, deletes ${when(a.deletes_at)}` : ""}`}
              aria-current={a.slug === r.app ? "page" : undefined}
              onclick={(e: Event) => {
                if (a.slug !== r.app) {
                  linkTo(e, () => navigate({ app: a.slug, view: "overview" }));
                } else {
                  linkTo(e, () => {});
                }
              }}
            >
              <span class="indicator">
                <span
                  class={`indicator-item status rail-dot ${statusDot(a.status)}`}
                  aria-hidden="true"
                />
                <span class="avatar avatar-placeholder" aria-hidden="true">
                  <span class="flex h-11 w-11 items-center justify-center text-lg font-semibold">
                    {monogram(a.slug)}
                  </span>
                </span>
              </span>
            </a>
          ))}
          {active?.admin === true && (
            <a
              key={`admin-${active.slug}`}
              href={hrefFor(r, { app: active.slug, view: "overview" })}
              role="listitem"
              class="rail-tile tooltip tooltip-right is-active"
              data-tip={`${active.slug} · ${active.status} · Admin view`}
              title={`${active.slug} · ${active.status} · Admin view`}
              aria-label={`${active.slug}, ${active.status}, viewing as admin`}
              aria-current="page"
              onclick={(e: Event) => linkTo(e, () => navigate({ app: active.slug, view: "overview" }))}
            >
              <span class="indicator">
                <span
                  class={`indicator-item status rail-dot ${statusDot(active.status)}`}
                  aria-hidden="true"
                />
                <span class="avatar avatar-placeholder" aria-hidden="true">
                  <span class="flex h-11 w-11 items-center justify-center text-lg font-semibold">
                    {monogram(active.slug)}
                  </span>
                </span>
              </span>
            </a>
          )}
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
            <div class="dropdown-content card card-border bg-base-100 w-64 p-4 text-left" role="dialog" aria-label="Creating apps">
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
        <button
          type="button"
          class="rail-tile tooltip tooltip-right"
          data-tip="Search ⌘K"
          aria-label="Search (⌘K)"
          title="Search (⌘K)"
          onclick={() => patchUi({ paletteOpen: true })}
        >
          <SearchIcon />
        </button>
        <div class="rail-foot">
          <a class="rail-tile tooltip tooltip-right" data-tip="Docs" href="/" aria-label="Docs">
            <DocsIcon />
          </a>
          <div class={`dropdown dropdown-right dropdown-end${ui().accountOpen ? " dropdown-open" : ""}`}>
            <button
              type="button"
              class="tooltip tooltip-right rounded-[14px]"
              data-tip={email() ? `Account: ${email()}` : "Account"}
              aria-label={email() ? `Account: ${email()}` : "Account"}
              aria-expanded={ui().accountOpen}
              onclick={() => patchUi({ accountOpen: !ui().accountOpen })}
            >
              <span class="avatar avatar-placeholder">
                <span class="bg-base-300 flex h-11 w-11 items-center justify-center rounded-[14px] text-lg font-semibold">
                  <span aria-hidden="true">
                    {(email() || "?").slice(0, 1).toUpperCase()}
                  </span>
                </span>
              </span>
            </button>
            {ui().accountOpen && (
              <ul class="dropdown-content menu bg-base-100 rounded-box border border-base-300 w-60 min-w-0 p-2" role="menu" aria-label="Account">
                <li class="menu-title min-w-0">
                  <span class="block truncate" title={email()}>
                    {email() || "Account"}
                  </span>
                </li>
                <li>
                  <a
                    href={hrefFor(r, { view: "account", acctab: "account" })}
                    onclick={(e: Event) => {
                      linkTo(e, () => {
                        patchUi({ accountOpen: false });
                        navigate({ view: "account", acctab: "account" });
                      });
                    }}
                  >
                    <AccountIcon size={16} />
                    Account
                  </a>
                </li>
                {isAdmin() && (
                  <li>
                    <a
                      href={hrefFor(r, { view: "admin" })}
                      onclick={(e: Event) => {
                        linkTo(e, () => {
                          patchUi({ accountOpen: false });
                          navigate({ view: "admin" });
                        });
                      }}
                    >
                      <ShieldIcon size={16} />
                      Admin
                    </a>
                  </li>
                )}
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

      {active && !accountView && (
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
                {ui().collapsed && (
                  <button
                    type="button"
                    class="btn btn-ghost btn-square btn-sm hidden shrink-0 md:inline-flex"
                    title="Expand sidebar"
                    aria-label="Expand sidebar"
                    onclick={() => patchUi({ collapsed: false })}
                  >
                    <ExpandIcon />
                  </button>
                )}
                <button
                  type="button"
                  class="btn btn-ghost btn-square btn-sm shrink-0 md:hidden"
                  title="Open navigation"
                  aria-label="Open navigation"
                  onclick={() => patchUi({ drawer: true })}
                >
                  <MenuIcon />
                </button>
                <h1 class="main-title" title={VIEW_TITLES[r.view]}>{VIEW_TITLES[r.view]}</h1>
                {(r.view === "overview" || r.view === "metrics") && (
                  <div class="join border border-base-300 ml-auto shrink-0 rounded-full p-1" role="group" aria-label="Metrics range">
                    {RANGES.map((opt) => (
                      <a
                        key={opt.hours}
                        href={hrefFor(r, { range: opt.hours })}
                        class={`btn btn-sm join-item border-0 rounded-full${r.range === opt.hours ? " bg-base-200" : " btn-ghost"}`}
                        title={opt.title}
                        aria-pressed={r.range === opt.hours}
                        onclick={(e: Event) => linkTo(e, () => navigate({ range: opt.hours }))}
                      >
                        {opt.label}
                      </a>
                    ))}
                  </div>
                )}
              </div>
              <div class="tabs tabs-box tabs-sm mb-3 w-full min-w-0 overflow-x-auto md:hidden" role="tablist" aria-label="Views">
                {VIEWS.filter((v) => v.id !== "d1").map((v) => (
                  <a
                    key={v.id}
                    href={hrefFor(r, { view: v.id as ViewId, codeTab: null })}
                    role="tab"
                    aria-selected={r.view === v.id}
                    class={`tab shrink-0${r.view === v.id ? " tab-active" : ""}`}
                    onclick={(e: Event) => linkTo(e, () => navigate({ view: v.id as ViewId, codeTab: null }))}
                  >
                    {v.label}
                  </a>
                ))}
              </div>
              <div class="main-body">
                {!accountView && detailPanel.kind === "loading" && (
                  <div class="flex min-w-0 flex-col gap-3" role="status" aria-label="Loading app">
                    <div class="skeleton rounded-box h-30 w-full" />
                    <div class="skeleton rounded-box h-70 w-full" />
                    <div class="skeleton rounded-box h-30 w-full" />
                  </div>
                )}
                {!accountView && detailPanel.kind === "error" &&
                  (isAdmin() && r.view === "code" && r.app
                    ? (
                      <div class="flex min-w-0 flex-col gap-3">
                        <div class="alert alert-soft min-w-0" role="note">
                          <span class="min-w-0">
                            Inspecting <strong class="truncate" title={r.app}>{r.app}</strong> as admin (read-only — every read is audited).
                          </span>
                        </div>
                        <CodeView
                          key={r.sha ?? "live"}
                          slug={r.app}
                          deploys={[]}
                          initialSha={r.sha}
                          initialTab={r.codeTab ?? "files"}
                          onPickSha={(sha) => navigate({ sha }, true)}
                          onSwitchTab={(codeTab) => navigate({ codeTab }, true)}
                          admin
                        />
                      </div>
                    )
                    : (
                      <div class="alert alert-error alert-soft" role="alert">
                        <span class="min-w-0">Couldn't load this app (unknown or not yours).</span>
                        {list.length > 0 ? (
                          <a
                            class="btn btn-sm min-w-0 max-w-full shrink-0"
                            href={hrefFor(r, { app: list[0].slug, view: "overview" })}
                            onclick={(e: Event) => linkTo(e, () => navigate({ app: list[0].slug, view: "overview" }))}
                          >
                            <span class="truncate" title={`Back to ${list[0].slug}`}>
                              Back to {list[0].slug}
                            </span>
                          </a>
                        ) : (
                          <a
                            class="btn btn-sm min-w-0 max-w-full shrink-0"
                            href={hrefFor(r, isAdmin() ? { app: null, view: "admin" } : { app: null, view: "account", acctab: "account" })}
                            onclick={(e: Event) => linkTo(e, () => navigate(isAdmin() ? { app: null, view: "admin" } : { app: null, view: "account", acctab: "account" }))}
                          >
                            <span class="truncate" title={isAdmin() ? "Back to Admin" : "Back to Account"}>
                              {isAdmin() ? "Back to Admin" : "Back to Account"}
                            </span>
                          </a>
                        )}
                      </div>
                    ))}
                {!accountView && detailPanel.kind === "ready" && (
                  <>
                    {detailPanel.value.admin === true && (
                      <div class="alert alert-soft alert-warning min-w-0 mb-3" role="note">
                        <span class="min-w-0">
                          Admin view of <strong class="truncate" title={detailPanel.value.slug}>{detailPanel.value.slug}</strong> — every read and mutation is audited.
                        </span>
                      </div>
                    )}
                    <ViewBody
                    route={r}
                    onView={(view) => navigate({ view })}
                    viewHref={(view) => hrefFor(route(), { view })}
                    onRoute={(next) => navigate(next)}
                    app={detailPanel.value}
                    slug={r.app as string}
                    metrics={metrics()}
                    actions={actions()}
                    errors={errors()}
                    logs={logs()}
                    secrets={secrets()}
                    collaborators={collaborators()}
                    domains={domains()}
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
                    onPickSha={(sha) => navigate({ sha }, true)}
                    onShowChanges={(sha) => navigate({ view: "code", sha, codeTab: "changes" })}
                    onSwitchCodeTab={(codeTab) => navigate({ codeTab }, true)}
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
                    onJumpToIssue={(fingerprint) => {
                      pendingJump.set(fingerprint);
                      navigate({ view: "errors" });
                    }}
                    jumpToIssue={pendingJump()}
                    onJumpConsumed={() => pendingJump.set(null)}
                    onRetry={reload}
                    onRefreshDetail={refreshDetail}
                    onRenamed={afterRename}
                    onDeleted={afterDelete}
                    plan={plan()}
                    onCollaboratorsChanged={() => {
                      const rr = route();
                      // Refresh the card through the same quiet path as the
                      // 10 s poll: no skeleton flash, epoch-guarded.
                      if (rr.app) {
                        void loadViewData(rr.app, "settings", rr.range, epoch, true);
                      }
                    }}
                    onAddDomain={async (hostname: string) => {
                      const slug = route().app;
                      if (!slug) {
                        return;
                      }
                      await domainActions.addDomain.run({ app: slug, hostname });
                      const summary = await domainActions.listDomains.run({ app: slug });
                      domains.set({ kind: "ready", value: summary });
                    }}
                    onRefreshDomains={async () => {
                      const slug = route().app;
                      if (!slug) {
                        return;
                      }
                      const res = await domainActions.refreshDomains.run({ app: slug });
                      const summary = await domainActions.listDomains.run({ app: slug });
                      // Refresh carries the live domain; `null` means removed.
                      if (res.domain) {
                        const current = domains();
                        if (current.kind === "ready") {
                          domains.set({
                            kind: "ready",
                            value: { ...summary, domains: [res.domain] },
                          });
                          return;
                        }
                      }
                      domains.set({ kind: "ready", value: summary });
                    }}
                    onRemoveDomain={async () => {
                      const slug = route().app;
                      if (!slug) {
                        return;
                      }
                      await domainActions.removeDomain.run({ app: slug });
                      const summary = await domainActions.listDomains.run({ app: slug });
                      domains.set({ kind: "ready", value: summary });
                    }}
                  />
                  </>
                )}
              </div>
            </main>
          </div>
          <div class="drawer-side">
            <label for="console-nav" class="drawer-overlay" aria-label="Close navigation" />
            <aside class="side max-md:min-h-dvh min-w-0" aria-label="App navigation">
              <div class="side-head">
                <div class="side-id">
                  <p class="m-0 min-w-0 truncate text-base font-semibold" title={active.slug}>
                    {active.slug}
                  </p>
                  <p class="m-0 min-w-0 truncate text-xs opacity-60" title={active.url}>
                    {active.url.replace(/^https?:\/\//, "")}
                  </p>
                </div>
                <span class={`badge badge-soft ${statusBadge(active.status)} shrink-0`} title={active.status}>{active.status}</span>
                {active.role === "collaborator" && (
                  <span class="badge badge-soft badge-info shrink-0" title="Shared with you — you can change this app, but only the owner manages it">Shared</span>
                )}
                {active.admin === true && (
                  <span class="badge badge-soft badge-warning shrink-0" title="Viewing as admin — every read and mutation is audited">Admin view</span>
                )}
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
                <li class="menu-title text-xs font-medium uppercase tracking-wider">App</li>
                {VIEWS.filter((v) => v.id !== "d1").map((v) => (
                  <li key={v.id} class="min-w-0">
                    <a
                      href={hrefFor(r, { view: v.id as ViewId, codeTab: null })}
                      class={r.view === v.id ? "menu-active min-w-0" : "min-w-0"}
                      aria-current={r.view === v.id ? "page" : undefined}
                      onclick={(e: Event) => linkTo(e, () => navigate({ view: v.id as ViewId, codeTab: null }))}
                    >
                      <ViewIcon id={v.id as ViewId} />
                      <span class="min-w-0 flex-1 truncate">{v.label}</span>
                      {v.id === "errors" && openErrors > 0 && (
                        <span class="badge badge-neutral badge-sm ml-auto shrink-0" title={`${openErrors} open errors`} aria-label={`${openErrors} open errors`}>
                          {openErrors}
                        </span>
                      )}
                    </a>
                  </li>
                ))}
                <li class="menu-title text-xs font-medium uppercase tracking-wider">Data</li>
                {/* R2, Durable Objects and D1 are owner-only in v1 (every endpoint 403s for collaborators). */}
                {active.role === "owner" && (
                  <>
                    <D1Nav
                      active={r.view === "d1"}
                      href={hrefFor(r, { view: "d1" })}
                      setRoute={(next) => navigate(next)}
                    />
                    {DATA_VIEWS.map((v) => (
                      <li key={v.id} class="min-w-0">
                        <a
                          href={hrefFor(r, { view: v.id })}
                          class={r.view === v.id ? "menu-active min-w-0" : "min-w-0"}
                          aria-current={r.view === v.id ? "page" : undefined}
                          onclick={(e: Event) => linkTo(e, () => navigate({ view: v.id }))}
                        >
                          <DatabaseIcon />
                          <span class="min-w-0 flex-1 truncate">{v.label}</span>
                        </a>
                      </li>
                    ))}
                  </>
                )}
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
      {r.view === "account" && (
        <div class={`drawer min-w-0${ui().collapsed ? "" : " md:drawer-open"}`}>
          <input
            id="console-nav"
            type="checkbox"
            class="drawer-toggle"
            checked={ui().drawer}
            aria-label="Account navigation toggle"
            onchange={(e: Event) => patchUi({ drawer: (e.target as HTMLInputElement).checked })}
          />
          <div class="drawer-content min-w-0">
            <main class="main" id="console-main" tabindex={-1}>
              <div class="main-head">
                {ui().collapsed && (
                  <button
                    type="button"
                    class="btn btn-ghost btn-square btn-sm hidden shrink-0 md:inline-flex"
                    title="Expand sidebar"
                    aria-label="Expand sidebar"
                    onclick={() => patchUi({ collapsed: false })}
                  >
                    <ExpandIcon />
                  </button>
                )}
                <button
                  type="button"
                  class="btn btn-ghost btn-square btn-sm shrink-0 md:hidden"
                  title="Open navigation"
                  aria-label="Open navigation"
                  onclick={() => patchUi({ drawer: true })}
                >
                  <MenuIcon />
                </button>
                <h1 class="main-title" title={hubTitle(r)}>{hubTitle(r)}</h1>
              </div>
              <div class="tabs tabs-box tabs-sm mb-3 w-full min-w-0 overflow-x-auto md:hidden" role="tablist" aria-label="Account">
                {ACCOUNT_TABS.map((t) => (
                  <a
                    key={t.id}
                    href={hrefFor(r, { view: "account", acctab: t.id })}
                    role="tab"
                    aria-selected={r.acctab === t.id}
                    class={`tab shrink-0${r.acctab === t.id ? " tab-active" : ""}`}
                    onclick={(e: Event) => linkTo(e, () => navigate({ view: "account", acctab: t.id }))}
                  >
                    {t.label}
                  </a>
                ))}
              </div>
              <div class="main-body">{hubPanel}</div>
            </main>
          </div>
          <div class="drawer-side">
            <label for="console-nav" class="drawer-overlay" aria-label="Close navigation" />
            <aside class="side max-md:min-h-dvh min-w-0" aria-label="Account navigation">
              <div class="side-head">
                <div class="side-id">
                  <p class="m-0 min-w-0 truncate text-base font-semibold" title="Account">
                    Account
                  </p>
                  <p class="m-0 min-w-0 truncate text-xs opacity-60" title={email()}>
                    {email() || "Signed in"}
                  </p>
                </div>
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
                <li class="menu-title text-xs font-medium uppercase tracking-wider">Account</li>
                {ACCOUNT_TABS.map((t) => (
                  <li key={t.id} class="min-w-0">
                    <a
                      href={hrefFor(r, { view: "account", acctab: t.id })}
                      class={r.acctab === t.id ? "menu-active min-w-0" : "min-w-0"}
                      aria-current={r.acctab === t.id ? "page" : undefined}
                      onclick={(e: Event) => linkTo(e, () => navigate({ view: "account", acctab: t.id }))}
                    >
                      <ViewIcon id={t.id} />
                      <span class="min-w-0 flex-1 truncate">{t.label}</span>
                    </a>
                  </li>
                ))}
              </ul>
              <div class="side-foot">
                <div class="divider my-2" aria-hidden="true" />
                <p class="m-0 min-w-0 truncate text-xs opacity-60" title={email()}>
                  {email()}
                </p>
              </div>
            </aside>
          </div>
        </div>
      )}
      {r.view === "admin" && (
        <div class="drawer min-w-0">
          <div class="drawer-content min-w-0">
            <main class="main" id="console-main" tabindex={-1}>
              <div class="main-head">
                <h1 class="main-title" title={VIEW_TITLES[r.view]}>{VIEW_TITLES[r.view]}</h1>
              </div>
              <div class="main-body">{accountPanel}</div>
            </main>
          </div>
        </div>
      )}
      {ui().paletteOpen && (
        <Palette
          apps={list}
          slug={r.app}
          view={r.view}
          isAdmin={isAdmin()}
          onNavigate={(next) => navigate(next)}
          execCtx={webmcpCtx}
          onMutated={() => {
            void reloadApps();
            refreshDetail();
          }}
          onClose={() => patchUi({ paletteOpen: false })}
        />
      )}
      {pendingConfirm() && (
        <div class="modal modal-open" role="dialog" aria-modal="true" aria-label={`Confirm ${pendingConfirm()?.action.title ?? "action"}`}>
          <div class="modal-box min-w-0">
            <h3 class="mb-2 min-w-0 truncate text-base font-semibold" title={pendingConfirm()?.action.title}>
              Run "{pendingConfirm()?.action.title}"?
            </h3>
            <p class="m-0 min-w-0 break-words text-sm opacity-70">
              An in-browser agent asked to run this consequential action as
              you. Target: <code class="font-mono">{confirmTargetOf(pendingConfirm()?.input ?? {})}</code>.
              Confirm only if you asked for this.
            </p>
            <details class="mt-2 min-w-0 text-sm">
              <summary class="cursor-pointer opacity-70">Input</summary>
              <pre class="mockup-code mt-1 max-h-40 overflow-auto text-xs">{JSON.stringify(pendingConfirm()?.input, null, 2)}</pre>
            </details>
            <div class="modal-action">
              <button
                type="button"
                class="btn btn-ghost"
                onclick={() => pendingConfirm()?.reject(new Error(`"${pendingConfirm()?.action.name}" was cancelled — nothing ran.`))}
              >
                Cancel
              </button>
              <button
                type="button"
                class="btn btn-error"
                onclick={() => pendingConfirm()?.resolve()}
              >
                Confirm
              </button>
            </div>
          </div>
          <button type="button" class="modal-backdrop" aria-label="Close" onclick={() => pendingConfirm()?.reject(new Error(`"${pendingConfirm()?.action.name}" was cancelled — nothing ran.`))}>
            Close
          </button>
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
    case "code":
      return <CodeIcon />;
    case "logs":
      return <LogsIcon />;
    case "errors":
      return <ErrorsIcon />;
    case "secrets":
      return <SecretsIcon />;
    case "settings":
      return <SettingsIcon />;
    case "d1":
      return <DatabaseIcon />;
    case "invites":
      return <TicketIcon />;
    case "admin":
      return <ShieldIcon />;
    case "connections":
      return <SecretsIcon />;
    case "usage":
      return <MetricsIcon />;
    case "account":
      return <AccountIcon />;
    case "r2":
    case "do":
      return <DatabaseIcon />;
  }
}

function ViewBody(props: {
  route: Route;
  app: AppDetail;
  plan: string;
  slug: string;
  metrics: Panel<{ hours: number; summary: MetricsSummary }>;
  actions: ActionsPanel;
  errors: Panel<ErrorsSummary>;
  logs: Panel<LogLine[]>;
  secrets: Panel<SecretsSummary>;
  collaborators: Panel<CollaboratorsSummary>;
  domains: Panel<DomainsSummary>;
  expanded: string[];
  onToggleExpand: (id: string) => void;
  onLoadMore: () => void;
  onRefreshLogs: () => void;
  onPickSha: (sha: string | null) => void;
  onShowChanges: (sha: string) => void;
  onSwitchCodeTab: (tab: "files" | "changes") => void;
  onJumpToIssue: (fingerprint: string) => void;
  /** A Logs "View issue" pick for the Errors view to expand, then consume. */
  jumpToIssue: string | null;
  onJumpConsumed: () => void;
  onCollaboratorsChanged: () => void;
  onAddDomain: (hostname: string) => Promise<void>;
  onRefreshDomains: () => Promise<void>;
  onRemoveDomain: () => Promise<void>;
  onRetry: () => void;
  onRefreshDetail: () => void;
  onRenamed: (slug: string) => void;
  onDeleted: () => void;
  /** Switch the current app's view (ViewBody is outside `navigate`'s scope). */
  onView: (view: ViewId) => void;
  /** Canonical URL for a view, for the overview's "View all" links. */
  viewHref: (view: ViewId) => string;
  /** Patch the route (views with their own URL state, e.g. D1). */
  onRoute: (next: Partial<Route>) => void;
}) {
  switch (props.route.view) {
    case "overview":
      return (
        <OverviewView
          app={props.app}
          metrics={props.metrics}
          actions={props.actions}
          errors={props.errors}
          onView={props.onView}
          viewHref={props.viewHref}
          onRetry={props.onRetry}
        />
      );
    case "metrics":
      return <MetricsView metrics={props.metrics} onRetry={props.onRetry} />;
    case "actions":
      return (
        <ActionsView
          panel={props.actions}
          onMore={props.onLoadMore}
          onRetry={props.onRetry}
        />
      );
    case "deploys":
      return (
        <DeploysView
          app={props.app}
          expanded={props.expanded}
          onToggle={props.onToggleExpand}
          onRolledBack={props.onRefreshDetail}
          onShowChanges={props.onShowChanges}
        />
      );
    case "code":
      return (
        <CodeView
          key={props.route.sha ?? "live"}
          slug={props.app.slug}
          deploys={props.app.deploys}
          initialSha={props.route.sha}
          initialTab={props.route.codeTab ?? "files"}
          onPickSha={props.onPickSha}
          onSwitchTab={props.onSwitchCodeTab}
        />
      );
    case "logs":
      return (
        <LogsView
          slug={props.slug}
          panel={props.logs}
          onRetry={props.onRetry}
          onJumpToIssue={props.onJumpToIssue}
        />
      );
    case "errors":
      return (
        <ErrorsView
          slug={props.slug}
          panel={props.errors}
          onRetry={props.onRetry}
          jump={props.jumpToIssue}
          onJumpConsumed={props.onJumpConsumed}
        />
      );
    case "secrets":
      return (
        <SecretsView
          canWrite={props.app.role === "owner"}
          slug={props.app.slug}
          panel={props.secrets}
          onRetry={props.onRetry}
          onChanged={props.onRetry}
        />
      );
    case "settings":
      return (
        <SettingsView
          app={props.app}
          plan={props.plan}
          onRefresh={props.onRefreshDetail}
          onRenamed={props.onRenamed}
          onDeleted={props.onDeleted}
          collaborators={props.collaborators}
          onCollaboratorsRetry={props.onRetry}
          onCollaboratorsChanged={props.onCollaboratorsChanged}
          domains={props.domains}
          onAddDomain={props.onAddDomain}
          onRefreshDomains={props.onRefreshDomains}
          onRemoveDomain={props.onRemoveDomain}
          onRetryDomains={props.onRetry}
        />
      );
    case "d1":
      return (
      props.app.role !== "owner" ? (
        <EmptyState icon={<DatabaseIcon />} title="D1 is owner-only">
          Only the app owner can open this database for now.
        </EmptyState>
      ) : (
      <D1View
        canWrite
        route={props.route}
        setRoute={props.onRoute}
        slug={props.app.slug}
      />
      )
      );
    case "r2":
      return (
      props.app.role !== "owner" ? (
        <EmptyState icon={<DatabaseIcon />} title="R2 is owner-only">
          Only the app owner can open this bucket browser for now.
        </EmptyState>
      ) : (
      <R2View slug={props.app.slug} />
      )
      );
    case "do":
      return (
      props.app.role !== "owner" ? (
        <EmptyState icon={<DatabaseIcon />} title="Durable Objects are owner-only">
          Only the app owner can open this panel for now.
        </EmptyState>
      ) : (
      <DoView slug={props.app.slug} />
      )
      );
  }
}
