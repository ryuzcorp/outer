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

export const ACCOUNT_VIEWS = [
  { id: "connections", label: "Connections & keys" },
  { id: "usage", label: "Usage & plan" },
  { id: "account", label: "Account" },
  { id: "invites", label: "Invite codes" },
  { id: "admin", label: "Admin" },
] as const;

export type AccountViewId = (typeof ACCOUNT_VIEWS)[number]["id"];

export type ViewId = (typeof VIEWS)[number]["id"] | AccountViewId;

export const RANGES = [
  { hours: 24, label: "24h", title: "Last 24 hours" },
  { hours: 168, label: "7d", title: "Last 7 days" },
  { hours: 720, label: "30d", title: "Last 30 days" },
] as const;

export interface Route {
  app: string | null;
  view: ViewId;
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
  if (VIEWS.some((v) => v.id === raw) || ACCOUNT_VIEWS.some((v) => v.id === raw)) {
    return raw as ViewId;
  }
  return "overview";
}

export function toRange(raw: string | null): number {
  const hours = Number(raw);
  return RANGES.some((r) => r.hours === hours) ? hours : 24;
}

export function readRoute(): Route {
  if (typeof window === "undefined") {
    return { app: null, view: "overview", range: 24, sha: null, codeTab: null, ...D1_DEFAULTS };
  }
  const q = new URLSearchParams(window.location.search);
  const ct = q.get("ct");
  return {
    app: q.get("app"),
    view: toViewId(q.get("t")),
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
