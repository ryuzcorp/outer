// Explicit lifecycle for the console shell's per-view loaders.
//
// Every loader in `console.tsx` builds one `DataKey` per invocation. A
// response lands only when its key still matches the resource's current
// key, so late polls and cross-view responses cannot repaint the panel.
// Quiet (background poll) failures keep the last value and surface as
// `stale` instead of silently passing as live.
/** Identity of one view's data: a response must match all of it to land. */
export interface DataKey {
  slug: string;
  view: string;
  range: number;
  epoch: number;
}

export function sameKey(a: DataKey, b: DataKey): boolean {
  return (
    a.slug === b.slug &&
    a.view === b.view &&
    a.range === b.range &&
    a.epoch === b.epoch
  );
}

/** A request's lifecycle, explicit: `idle` (never loaded), `loading` (first
 *  load, the view shows its skeleton), `ready`, `stale` (last value kept, the
 *  newest refresh failed) and `failed` (no value to show). */
export type FetchState =
  | { readonly status: "idle" }
  | { readonly status: "loading" }
  | { readonly status: "ready"; readonly at: number }
  | { readonly status: "stale"; readonly at: number; readonly message: string }
  | { readonly status: "failed"; readonly message: string };

export type FetchAction =
  | { type: "began"; key: DataKey; quiet: boolean }
  | { type: "succeeded"; key: DataKey; at: number }
  | { type: "failed"; key: DataKey; message: string };

/** Pure transition: ignores any action whose key is not the current one, so
 *  late and cross-view responses cannot land. `quiet` reloads never leave
 *  `ready`/`stale` (no skeleton flash); a failed quiet reload of a loaded
 *  view becomes `stale`, a failed first load becomes `failed`. */
export function reduceFetch(
  state: FetchState,
  current: DataKey | null,
  action: FetchAction,
): FetchState {
  if (current === null || !sameKey(current, action.key)) {
    return state;
  }
  switch (action.type) {
    case "began":
      return action.quiet ? state : { status: "loading" };
    case "succeeded":
      return { status: "ready", at: action.at };
    case "failed":
      if (state.status === "ready" || state.status === "stale") {
        return { status: "stale", at: state.at, message: action.message };
      }
      return { status: "failed", message: action.message };
  }
}

export type ResourceId =
  | "metrics"
  | "actions"
  | "errors"
  | "logs"
  | "secrets"
  | "collaborators"
  | "domains";

export interface Registry {
  keys: Record<string, DataKey | null>;
  states: Record<string, FetchState>;
}

export function emptyRegistry(): Registry {
  return { keys: {}, states: {} };
}

function stateOf(r: Registry, id: string): FetchState {
  return r.states[id] ?? { status: "idle" };
}

/** Record a new fetch for `id` and return whether the caller may proceed
 *  (always true today; the value is the key it recorded). Pure: the input
 *  registry is never mutated. */
export function startFetch(
  r: Registry,
  id: string,
  key: DataKey,
  quiet: boolean,
): { registry: Registry; run: boolean } {
  return {
    registry: {
      keys: { ...r.keys, [id]: key },
      states: {
        ...r.states,
        [id]: reduceFetch(stateOf(r, id), key, { type: "began", key, quiet }),
      },
    },
    run: true,
  };
}

/** Settle a fetch: `ok` records `ready`, a failure records `stale` when a
 *  value is showing and `failed` otherwise. Responses whose key is not the
 *  resource's current key are dropped (the input registry is returned
 *  unchanged, by identity). */
export function settleFetch(
  r: Registry,
  id: string,
  key: DataKey,
  ok: boolean,
  message?: string,
  at?: number,
): Registry {
  const current = r.keys[id] ?? null;
  if (current === null || !sameKey(current, key)) {
    return r;
  }
  const action: FetchAction = ok
    ? { type: "succeeded", key, at: at ?? Date.now() }
    : { type: "failed", key, message: message ?? "fetch failed" };
  const next = reduceFetch(stateOf(r, id), current, action);
  if (next === stateOf(r, id)) {
    return r;
  }
  return { keys: r.keys, states: { ...r.states, [id]: next } };
}

/** The first `stale` message among `ids`, or null when none of them is
 *  stale. The shell shows its fixed banner copy when this is non-null. */
export function staleMessage(r: Registry, ids: string[]): string | null {
  for (const id of ids) {
    const state = r.states[id];
    if (state && state.status === "stale") {
      return state.message;
    }
  }
  return null;
}
