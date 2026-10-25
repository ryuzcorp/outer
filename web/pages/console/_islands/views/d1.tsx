import { atom } from "ilha";

import { d1run } from "../actions/data-d1.ts";
import type {
  D1DatabasesInput,
  D1DatabasesOutput,
} from "../actions/data-d1.ts";
import type { Route } from "../common.tsx";
import { DatabaseIcon } from "../icons.tsx";
import { D1Editor } from "../data/d1/panel.tsx";
import { DataEmptyState } from "../data/shared.tsx";

type DatabasesState =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; value: D1DatabasesOutput };

/** The app's declared D1 bindings, fetched once per mount. */
const useDatabases = (slug: string): DatabasesState & { retry: () => void } => {
  const state = atom<DatabasesState>({ kind: "loading" });
  const started = atom(false);

  const load = () => {
    started.set(true);
    state.set({ kind: "loading" });
    const input: D1DatabasesInput = { slug };
    d1run<D1DatabasesInput, D1DatabasesOutput>("d1_databases", input).then(
      (value) => {
        state.set({ kind: "ready", value });
      },
      (error: unknown) => {
        state.set({
          kind: "error",
          message: error instanceof Error ? error.message : String(error),
        });
      },
    );
  };

  if (!started()) {
    load();
  }

  const current = state();
  return {
    ...current,
    retry: () => {
      started.set(false);
    },
  };
};

/** `d1` view body: database picker, editor, and empty states. */
export function D1View({
  canWrite,
  route,
  setRoute,
  slug,
}: {
  /** False for collaborators: D1 editing stays owner-only. */
  canWrite: boolean;
  route: Route;
  setRoute: (next: Partial<Route>) => void;
  slug: string;
}) {
  const databases = useDatabases(slug);
  // Remount the whole editor when the app or database changes, so table
  // and schema state never leaks across them.
  const editorKey = `${slug}:${route.db}`;

  if (databases.kind === "error") {
    return (
      <div class="flex min-w-0 flex-col gap-3">
        <section class="card card-border bg-base-100 min-w-0" aria-label="D1">
          <div class="card-body min-w-0">
            <h2 class="card-title min-w-0 truncate" title="D1">
              D1
            </h2>
            <div class="alert alert-error alert-soft" role="alert">
              <span class="min-w-0">{databases.message}</span>
            </div>
            <div>
              <button
                type="button"
                class="btn btn-sm"
                onclick={databases.retry}
              >
                Retry
              </button>
            </div>
          </div>
        </section>
      </div>
    );
  }

  if (databases.kind === "loading") {
    return (
      <div class="flex min-w-0 flex-col gap-3" aria-label="Loading D1">
        <div class="card card-border bg-base-100 min-w-0">
          <div class="card-body min-w-0 gap-2">
            <div class="skeleton h-7 w-40" />
            <div class="skeleton h-6 w-full" />
            <div class="skeleton h-24 w-full" />
          </div>
        </div>
      </div>
    );
  }

  const list = databases.value.databases;
  const binding =
    list.find((candidate) => candidate.binding === route.db)?.binding ??
    list[0]?.binding;

  if (binding === undefined) {
    return (
      <div class="flex min-w-0 flex-col gap-3">
        <section class="card card-border bg-base-100 min-w-0" aria-label="D1">
          <div class="card-body min-w-0">
            <h2 class="card-title min-w-0 truncate" title="D1">
              D1
            </h2>
            <DataEmptyState
              title="No D1 databases yet"
              hint="Add a d1_databases binding to the app config and deploy, then browse tables here. The D1 guide in Docs shows the config."
              action={
                <a class="btn btn-sm" href="/docs/guides/d1">
                  D1 guide
                </a>
              }
            />
          </div>
        </section>
      </div>
    );
  }

  return (
    <div class="flex min-w-0 flex-col gap-3">
      {list.length > 1 ? (
        <div
          class="join flex-wrap"
          role="group"
          aria-label="D1 database"
        >
          {list.map((candidate) => (
            <button
              key={candidate.binding}
              type="button"
              class={`btn btn-sm join-item ${candidate.binding === binding ? "btn-active" : ""}`}
              aria-pressed={candidate.binding === binding ? "true" : "false"}
              title={candidate.database_name}
              onclick={() => {
                setRoute({
                  db: candidate.binding,
                  table: "",
                  f: "",
                  sort: "",
                  q: "",
                  page: "",
                  dview: "",
                });
              }}
            >
              {candidate.binding}
            </button>
          ))}
        </div>
      ) : null}
      {canWrite ? null : (
        <div class="alert alert-info alert-soft min-w-0" role="note">
          <span class="min-w-0 break-words">
            D1 editing is owner-only. You can browse tables and rows, but only
            the app owner can insert, edit, or delete rows.
          </span>
        </div>
      )}
      <section
        class="card card-border bg-base-100 min-w-0 overflow-hidden"
        aria-label={`D1 database ${binding}`}
      >
        <div class="card-body min-w-0 gap-0 p-0">
          <D1Editor
            key={editorKey}
            binding={binding}
            canWrite={canWrite}
            route={route}
            setRoute={setRoute}
            slug={slug}
          />
        </div>
      </section>
    </div>
  );
}

/** Sidebar Data entry: the D1 row plus one sub-row per declared binding. */
export function D1Nav({
  active,
  route,
  setRoute,
  slug,
}: {
  /** Whether the D1 view is showing. */
  active: boolean;
  route: Route;
  setRoute: (next: Partial<Route>) => void;
  slug: string;
}) {
  const databases = useDatabases(slug);

  const open = (binding: string) => {
    setRoute({
      view: "d1",
      db: binding,
      table: "",
      f: "",
      sort: "",
      q: "",
      page: "",
      dview: "",
    });
  };

  const list = databases.kind === "ready" ? databases.value.databases : [];

  return (
    <>
      <li class="min-w-0">
        <button
          type="button"
          class={active ? "menu-active min-w-0" : "min-w-0"}
          aria-current={active ? "page" : undefined}
          onclick={() => {
            open(route.db || list[0]?.binding || "");
          }}
        >
          <DatabaseIcon />
          <span class="min-w-0 flex-1 truncate">D1</span>
          {list.length > 0 ? (
            <span
              class="badge badge-soft badge-sm ml-auto shrink-0"
              title={`${list.length} database${list.length === 1 ? "" : "s"}`}
            >
              {list.length}
            </span>
          ) : null}
        </button>
      </li>
      {active || list.length === 0
        ? null
        : list.map((candidate) => (
            <li key={candidate.binding} class="min-w-0">
              <button
                type="button"
                class="min-w-0 pl-11"
                title={candidate.database_name}
                onclick={() => {
                  open(candidate.binding);
                }}
              >
                <span class="min-w-0 flex-1 truncate">
                  {candidate.binding}
                </span>
              </button>
            </li>
          ))}
    </>
  );
}
