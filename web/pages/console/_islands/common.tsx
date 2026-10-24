import { atom } from "ilha";

import type { ActionEntry } from "./api.ts";
import { CheckIcon, CopyIcon } from "./icons.tsx";

export const VIEWS = [
  { id: "overview", label: "Overview" },
  { id: "metrics", label: "Metrics" },
  { id: "actions", label: "Agent actions" },
  { id: "deploys", label: "Deploys" },
  { id: "logs", label: "Logs" },
  { id: "errors", label: "Errors" },
  { id: "secrets", label: "Secrets" },
  { id: "settings", label: "Settings" },
] as const;

export type ViewId = (typeof VIEWS)[number]["id"];

export const RANGES = [
  { hours: 24, label: "24h", title: "Last 24 hours" },
  { hours: 168, label: "7d", title: "Last 7 days" },
  { hours: 720, label: "30d", title: "Last 30 days" },
] as const;

export interface Route {
  app: string | null;
  view: ViewId;
  range: number;
}

export function toViewId(raw: string | null): ViewId {
  return VIEWS.some((v) => v.id === raw) ? (raw as ViewId) : "overview";
}

export function toRange(raw: string | null): number {
  const hours = Number(raw);
  return RANGES.some((r) => r.hours === hours) ? hours : 24;
}

export function readRoute(): Route {
  if (typeof window === "undefined") {
    return { app: null, view: "overview", range: 24 };
  }
  const q = new URLSearchParams(window.location.search);
  return {
    app: q.get("app"),
    view: toViewId(q.get("t")),
    range: toRange(q.get("r")),
  };
}

export function routeUrl(route: Route): string {
  const q = new URLSearchParams();
  if (route.app) {
    q.set("app", route.app);
  }
  q.set("t", route.view);
  q.set("r", String(route.range));
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
export function CopyButton({ value, label }: { value: string; label: string }) {
  const copied = atom(false);
  return (
    <button
      type="button"
      class="icon-btn"
      title={label}
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
