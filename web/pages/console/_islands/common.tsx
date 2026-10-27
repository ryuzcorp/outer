import { atom } from "ilha";

import type { ActionEntry } from "./api.ts";
import { CheckIcon, CopyIcon } from "./icons.tsx";

export const VIEWS = [
  { id: "overview", label: "Overview" },
  { id: "metrics", label: "Metrics" },
  { id: "actions", label: "Agent actions" },
  { id: "deploys", label: "Deploys" },
  { id: "code", label: "Code" },
  { id: "logs", label: "Logs" },
  { id: "errors", label: "Errors" },
  { id: "secrets", label: "Secrets" },
  { id: "settings", label: "Settings" },
  { id: "d1", label: "D1" },
] as const;

export const ACCOUNT_TABS = [
  { id: "account", label: "Account" },
  { id: "usage", label: "Usage & plan" },
  { id: "connections", label: "Connections & keys" },
  { id: "invites", label: "Invite codes" },
] as const;

export type AccountTabId = (typeof ACCOUNT_TABS)[number]["id"];

export const ACCOUNT_VIEWS = [
  { id: "connections", label: "Connections & keys" },
  { id: "usage", label: "Usage & plan" },
  { id: "account", label: "Account" },
  { id: "invites", label: "Invite codes" },
  { id: "admin", label: "Admin" },
] as const;

export const DATA_VIEWS = [
  { id: "r2", label: "R2" },
  { id: "do", label: "Durable Objects" },
] as const;

export type AccountViewId = (typeof ACCOUNT_VIEWS)[number]["id"];

export type ViewId =
  | (typeof VIEWS)[number]["id"]
  | AccountViewId
  | (typeof DATA_VIEWS)[number]["id"];

export const RANGES = [
  { hours: 24, label: "24h", title: "Last 24 hours" },
  { hours: 168, label: "7d", title: "Last 7 days" },
  { hours: 720, label: "30d", title: "Last 30 days" },
] as const;

export interface Route {
  app: string | null;
  view: ViewId;
  // The Account hub's tab. Old top-level account URLs (`?t=usage`) map
  // onto it in readRoute so docs and email links keep landing on the
  // right tab. Other views ignore it; it survives view switches the way
  // `sha` does so going back to Account keeps its place.
  acctab: AccountTabId;
  range: number;
  // The Code view's pinned version; null means the live sha. Other views
  // ignore it, and it survives view switches so Code keeps its place.
  sha: string | null;
  // The Code view's section; null means files. The Deploys "what changed"
  // link sets it so the diff shows immediately.
  codeTab: "files" | "changes" | null;
  /** D1 editor state (only read while `view` is `d1`; see `data/d1/state`). */
  db: string;
  table: string;
  page: string;
  size: string;
  sort: string;
  f: string;
  q: string;
  dview: string;
}

const D1_DEFAULTS: Pick<Route, "db" | "table" | "page" | "size" | "sort" | "f" | "q" | "dview"> = {
  db: "",
  table: "",
  page: "",
  size: "",
  sort: "",
  f: "",
  q: "",
  dview: "",
};

export function toViewId(raw: string | null): ViewId {
  if (VIEWS.some((v) => v.id === raw) || ACCOUNT_VIEWS.some((v) => v.id === raw) || DATA_VIEWS.some((v) => v.id === raw)) {
    return raw as ViewId;
  }
  return "overview";
}

export function toAcctab(raw: string | null): AccountTabId {
  if (ACCOUNT_TABS.some((t) => t.id === raw)) {
    return raw as AccountTabId;
  }
  return "account";
}

/** Old top-level account views live on as hub tabs now (`account` is the
 * hub itself rather than a legacy view, so it maps to nothing: merging
 * `{ view: "account", acctab }` must keep the explicit tab). */
const LEGACY_ACCOUNT_TABS: Record<string, AccountTabId> = {
  usage: "usage",
  connections: "connections",
  invites: "invites",
};

/**
 * Map a legacy `?t=` account view onto the hub (`t=account` + `acctab`).
 * Returns null for views that navigate unchanged.
 */
export function toAccountRoute(view: ViewId): { view: "account"; acctab: AccountTabId } | null {
  const tab = LEGACY_ACCOUNT_TABS[view];
  return tab === undefined ? null : { view: "account", acctab: tab };
}

/**
 * Merge a route patch the way the shell's `navigate` does: the patch wins,
 * then a legacy top-level `?t=` view lands on its hub tab. Anchors use this
 * so their `href` always matches the SPA navigation.
 */
export function mergeRoute(prev: Route, next: Partial<Route>): Route {
  const r: Route = { ...prev, ...next };
  const mapped = toAccountRoute(r.view);
  if (mapped) {
    r.view = mapped.view;
    r.acctab = mapped.acctab;
  }
  return r;
}

/** The URL `navigate(next)` would push from `prev`. */
export function hrefFor(prev: Route, next: Partial<Route>): string {
  return routeUrl(mergeRoute(prev, next));
}

/**
 * Plain left-clicks run the SPA navigation; modified clicks and
 * middle-clicks fall through to the anchor so open-in-new-tab (and the
 * copied URL) keeps working.
 */
export function linkTo(e: Event, go: () => void): void {
  const m = e as MouseEvent;
  if (m.ctrlKey || m.metaKey || m.shiftKey || m.altKey || (m.button ?? 0) !== 0) {
    return;
  }
  e.preventDefault();
  go();
}


export function toRange(raw: string | null): number {
  const hours = Number(raw);
  return RANGES.some((r) => r.hours === hours) ? hours : 24;
}

export function readRoute(): Route {
  if (typeof window === "undefined") {
    return { app: null, view: "overview", acctab: "account", range: 24, sha: null, codeTab: null, ...D1_DEFAULTS };
  }
  const q = new URLSearchParams(window.location.search);
  const ct = q.get("ct");
  const view = toViewId(q.get("t"));
  // Old `?t=usage|connections|invites` links (docs, emails, the upgrade
  // flow) land on the matching hub tab. An explicit `?acctab=` wins:
  // the hub's own canonical URLs carry it.
  const mapped = view === "account" ? null : toAccountRoute(view);
  return {
    app: q.get("app"),
    view: mapped?.view ?? view,
    acctab: mapped?.acctab ?? toAcctab(q.get("acctab")),
    range: toRange(q.get("r")),
    sha: q.get("sha"),
    codeTab: ct === "changes" ? "changes" : ct === "files" ? "files" : null,
    db: q.get("db") ?? "",
    table: q.get("table") ?? "",
    page: q.get("page") ?? "",
    size: q.get("size") ?? "",
    sort: q.get("sort") ?? "",
    f: q.get("f") ?? "",
    q: q.get("q") ?? "",
    dview: q.get("dview") ?? "",
  };
}

export function routeUrl(route: Route): string {
  const q = new URLSearchParams();
  if (route.app) {
    q.set("app", route.app);
  }
  q.set("t", route.view);
  if (route.view === "account" && route.acctab !== "account") {
    q.set("acctab", route.acctab);
  }
  q.set("r", String(route.range));
  if (route.sha) {
    q.set("sha", route.sha);
  }
  if (route.codeTab) {
    q.set("ct", route.codeTab);
  }
  // The D1 editor's deep-link state rides alongside; other views ignore it.
  for (const key of ["db", "table", "page", "size", "sort", "f", "q", "dview"] as const) {
    if (route[key] !== "") {
      q.set(key, route[key]);
    }
  }
  return `/console?${q.toString()}`;
}

// Who may open an app page by slug. Admins may open apps outside their own
// rail (the backend serves foreign apps to admins and 404s truly-missing
// slugs, so the shell loads and lets the backend decide); everyone else
// only opens rail apps, so an unknown slug is a local 404 without a fetch.
export function canOpenApp(
  isAdmin: boolean,
  ownSlugs: readonly string[],
  slug: string,
): boolean {
  return isAdmin || ownSlugs.includes(slug);
}

// Relative times with the absolute time on hover (Noite's convention).
export function timeAgo(ms: number): string {
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

export type Panel<T> =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; value: T };

export type { ActionEntry };


export interface ActionsState {
  actions: ActionEntry[];
  next: string | null;
  loadingMore: boolean;
  moreError: string | null;
}

export type ActionsPanel =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; state: ActionsState };

// Copy button (Noite's $lib/ui/copy-button, per-instance feedback).
export function CopyButton({ value, label, class: klass }: { value: string; label: string; class?: string }) {
  const copied = atom(false);
  return (
    <button
      type="button"
      class={klass ?? "btn btn-ghost btn-square btn-xs tooltip shrink-0"}
      data-tip={copied() ? "Copied" : label}
      title={copied() ? "Copied" : label}
      aria-label={label}
      onclick={() => {
        const done = () => {
          copied.set(true);
          window.setTimeout(() => copied.set(false), 1500);
        };
        if (navigator.clipboard?.writeText) {
          navigator.clipboard.writeText(value).then(done, () => {});
        } else {
          const area = document.createElement("textarea");
          area.value = value;
          document.body.appendChild(area);
          area.select();
          document.execCommand("copy");
          document.body.removeChild(area);
          done();
        }
      }}
    >
      {copied() ? <CheckIcon /> : <CopyIcon />}
    </button>
  );
}
