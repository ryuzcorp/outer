// Shared chrome for the console data views (D1 editor, R2 browser, DO
// viewer): toasts, breadcrumbs, detail panels, empty states, confirms and
// preview helpers. Kept small and generic on purpose: the three panels
// should look like one product because they render these pieces, not three
// near-copies of them.

import { atom } from "ilha";

import { timeAgo } from "../common.tsx";

/** One transient confirmation (top-right, auto-hides). */
export interface Toast {
  id: number;
  text: string;
}

/** Toast state for one panel; `notify` shows a message for four seconds. */
export const useToasts = () => {
  const toasts = atom<Toast[]>([]);
  // Plain closure counter: the tuple this returns is fresh per component
  // instance, so no cross-instance id sharing.
  let next = 0;

  const notify = (text: string) => {
    next += 1;
    const id = next;
    toasts.set([...toasts(), { id, text }]);
    window.setTimeout(() => {
      toasts.set(toasts().filter((toast) => toast.id !== id));
    }, 4000);
  };

  return { notify, toasts };
};

export const Toaster = ({ toasts }: { toasts: Toast[] }) => (
  <div class="toast toast-top toast-end z-[100]">
    {toasts.map((toast) => (
      <div key={String(toast.id)} class="alert alert-success alert-soft">
        {toast.text}
      </div>
    ))}
  </div>
);

/** One breadcrumb step: a button when it leads somewhere, text when it is
 * the page the viewer is on. */
export interface Crumb {
  label: string;
  onPick?: () => void;
}

export function DataBreadcrumb({ crumbs }: { crumbs: Crumb[] }) {
  return (
    <nav aria-label="Breadcrumb" class="min-w-0">
      <ol class="flex min-w-0 flex-wrap items-center gap-x-1 gap-y-0.5 text-sm">
        {crumbs.map((crumb, index) => (
          <li key={index} class="flex min-w-0 items-center gap-1">
            {index > 0 ? (
              <span aria-hidden="true" class="opacity-40">
                /
              </span>
            ) : null}
            {crumb.onPick ? (
              <button
                type="button"
                class="link link-hover min-w-0 max-w-48 truncate opacity-70"
                title={crumb.label}
                onclick={crumb.onPick}
              >
                {crumb.label}
              </button>
            ) : (
              <span
                aria-current="page"
                class="min-w-0 max-w-48 truncate font-medium"
                title={crumb.label}
              >
                {crumb.label}
              </span>
            )}
          </li>
        ))}
      </ol>
    </nav>
  );
}

/** The right-hand detail column shared by the R2 and DO views: a titled
 * panel that sits beside the list on wide screens and stacks below it on
 * narrow ones. */
export function DetailPanel({
  children,
  title,
}: {
  children?: unknown;
  title: string;
}) {
  return (
    <aside
      aria-label="Details"
      class="border-base-300 flex w-full min-w-0 flex-col gap-3 border-t p-4 lg:w-96 lg:shrink-0 lg:border-t-0 lg:border-l"
    >
      <h2 class="m-0 min-w-0 truncate text-sm font-semibold" title={title}>
        {title}
      </h2>
      {children}
    </aside>
  );
}

/** The empty-list body: what is missing, why, and how to fill it. */
export function DataEmptyState({
  action,
  hint,
  title,
}: {
  action?: unknown;
  hint: string;
  title: string;
}) {
  return (
    <div class="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 p-10 text-center">
      <h3 class="m-0 text-base font-semibold">{title}</h3>
      <p class="m-0 max-w-md text-sm opacity-60">{hint}</p>
      {action}
    </div>
  );
}

/** Destructive confirm in a daisyUI modal: one sentence of consequence,
 * Delete + Cancel. Rendered conditionally (no dialog API, no refs), so it
 * stays CSP-clean. */
export function ConfirmModal({
  busy,
  busyLabel,
  confirmLabel,
  consequence,
  error,
  onCancel,
  onConfirm,
  title,
}: {
  busy: boolean;
  /** Busy button text; defaults to "Deleting…". */
  busyLabel?: string;
  /** Confirm button text; defaults to "Delete". */
  confirmLabel?: string;
  consequence: string;
  /** Failure from the confirmed action, rendered inside the dialog. */
  error?: string;
  onCancel: () => void;
  onConfirm: () => void;
  title: string;
}) {
  return (
    <div class="modal modal-open" role="dialog" aria-modal="true" aria-label={title}>
      <div class="modal-box min-w-0">
        <h3 class="m-0 text-base font-semibold">{title}</h3>
        <p class="m-0 py-3 text-sm opacity-80">{consequence}</p>
        {error === undefined || error === "" ? null : (
          <div class="alert alert-error alert-soft mb-3 text-sm" role="alert">
            <span>{error}</span>
          </div>
        )}
        <div class="modal-action">
          <button
            type="button"
            class="btn btn-ghost"
            disabled={busy}
            onclick={onCancel}
          >
            Cancel
          </button>
          <button
            type="button"
            class="btn btn-error"
            disabled={busy}
            onclick={onConfirm}
          >
            {busy ? (busyLabel ?? "Deleting…") : (confirmLabel ?? "Delete")}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Human sizes, one spelling everywhere (`12 KB`, `3.1 MB`). */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) {
    return "—";
  }
  if (n < 1024) {
    return `${n} B`;
  }
  const units = ["KB", "MB", "GB", "TB"];
  let value = n / 1024;
  let unit = units[0];
  for (const next of units) {
    unit = next;
    if (value < 1024 || next === "TB") {
      break;
    }
    value /= 1024;
  }
  const text = value >= 100 ? String(Math.round(value)) : value.toFixed(1).replace(/\.0$/, "");
  return `${text} ${unit}`;
}

/** Short local date-time for timestamp hints (D1 cells and the row editor
 * pass a `Date`; R2 passes the ISO string straight from S3). */
export function formatDateTime(value: Date | string): string {
  const date = typeof value === "string" ? new Date(value) : value;
  if (Number.isNaN(date.getTime())) {
    return typeof value === "string" ? value : String(value);
  }
  return date.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function formatAgo(iso: string): string {
  const ms = Date.parse(iso);
  if (Number.isNaN(ms)) {
    return iso;
  }
  return timeAgo(ms);
}

/** Parse an unknown thrown value into a display message. */
export function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message) {
    return error.message;
  }
  return String(error);
}

/** What the R2 detail panel can show for one object. */
export type PreviewKind = "image" | "text" | "binary";

export function previewKind(contentType: string | null): PreviewKind {
  if (contentType?.startsWith("image/")) {
    return "image";
  }
  if (
    contentType?.startsWith("text/") ||
    contentType?.includes("json") ||
    contentType?.includes("xml")
  ) {
    return "text";
  }
  return "binary";
}

/** Pretty-print a JSON body for a preview; non-JSON text is left as it is. */
export function prettyJson(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}
