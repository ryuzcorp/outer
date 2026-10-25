// URL-free state and pure helpers for the R2 browser: the view toggle, the
// breadcrumb path into a folder, and the selection map the multi-select bar
// reads. (Noite keeps this in the URL via searchParam; Outer's console
// routes by app view only, so the folder state lives in the island.)
// Everything here is free of DOM and store access so it stays unit-testable.

import type { Crumb } from "../shared.tsx";

export const R2_VIEWS = ["list", "columns"] as const;

export type R2View = (typeof R2_VIEWS)[number];

export const R2_VIEW_LABELS: Record<R2View, string> = {
  columns: "Columns",
  list: "List",
};

export const toR2View = (raw: string): R2View =>
  raw === "columns" ? "columns" : "list";

/** The breadcrumb path into one folder: bucket › segments. Steps above the
 * current folder navigate; the current folder is text. An empty segment
 * stays visible as "(empty)". */
export const r2Crumbs = (
  bucket: string,
  prefix: string,
  onPick: (prefix: string) => void,
): Crumb[] => {
  // A folder prefix always ends in `/`, so its parts are everything before
  // that last slash.
  const parts = prefix === "" ? [] : prefix.split("/").slice(0, -1);

  const crumbs: Crumb[] = [
    parts.length > 0 ? { label: bucket, onPick: () => onPick("") } : { label: bucket },
  ];

  for (const [index, part] of parts.entries()) {
    crumbs.push(
      index === parts.length - 1
        ? { label: part === "" ? "(empty)" : part }
        : {
            label: part === "" ? "(empty)" : part,
            onPick: () => onPick(`${parts.slice(0, index + 1).join("/")}/`),
          },
    );
  }

  return crumbs;
};

/** Flip one key in a selection map without mutating the caller's copy. */
export const toggledKey = (
  selection: ReadonlySet<string>,
  key: string,
  on: boolean,
): Set<string> => {
  const next = new Set(selection);

  if (on) {
    next.add(key);
  } else {
    next.delete(key);
  }

  return next;
};
