// Shared chrome for the console data views (D1 now; R2 and Durable
// Objects land here too): toasts, empty states and JSON pretty-printing.
// Kept small and generic on purpose: the D1 editor, R2 browser and DO
// viewer should look like one product because they render these pieces,
// not three near-copies of them.

import { atom } from "ilha";
import type { View } from "ilha";

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

/** The empty-list body: what is missing, why, and how to fill it. */
export const DataEmptyState = ({
  action,
  hint,
  title,
}: {
  action?: View;
  hint: string;
  title: string;
}) => (
  <div class="flex flex-1 flex-col items-center justify-center gap-2 p-10 text-center">
    <p class="m-0 font-medium">{title}</p>
    <p class="m-0 max-w-md text-sm opacity-60">{hint}</p>
    {action}
  </div>
);

/** Pretty-print a JSON body for a preview; non-JSON text is left as it is. */
export const prettyJson = (text: string): string => {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
};

/** Short local date-time for timestamp hints (cells + row editor share it). */
export const formatDateTime = (date: Date): string =>
  date.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
