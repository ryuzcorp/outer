import { atom } from "ilha";

import { d1run } from "../actions/data-d1.ts";
import type {
  D1DatabasesInput,
  D1DatabasesOutput,
} from "../actions/data-d1.ts";
import type { Route } from "../common.tsx";
import { DatabaseIcon } from "../icons.tsx";
import { BackupsPanel } from "../data/d1/backups.tsx";
import { D1Editor } from "../data/d1/panel.tsx";
import { DataEmptyState } from "../data/shared.tsx";

type DatabasesState =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; value: D1DatabasesOutput };

/** One in-flight (or finished) fetch per app. Failures evict the entry so
 * a retry fetches again. */
const databasesCache = new Map<string, Promise<D1DatabasesOutput>>();

const fetchDatabases = (slug: string): Promise<D1DatabasesOutput> => {
  const cached = databasesCache.get(slug);
  if (cached !== undefined) {
    return cached;
  }
  const pending = d1run<D1DatabasesInput, D1DatabasesOutput>("d1_databases", {
    slug,
  });
  databasesCache.set(slug, pending);
  pending.catch(() => {
    if (databasesCache.get(slug) === pending) {
      databasesCache.delete(slug);
    }
  });
  return pending;
};
/** The app's declared D1 binding, fetched once per app (not per mount). */
const useDatabases = (slug: string): DatabasesState & { retry: () => void } => {
  const state = atom<DatabasesState>({ kind: "loading" });
  const started = atom("");

  const load = () => {
    const key = slug;
    started.set(key);
    state.set({ kind: "loading" });
    fetchDatabases(slug).then(
      (value) => {
        if (started() === key) {
          state.set({ kind: "ready", value });
        }
      },
      (error: unknown) => {
        if (started() === key) {
          state.set({
            kind: "error",
            message: error instanceof Error ? error.message : String(error),
          });
        }
      },
    );
  };

  if (started() !== slug) {
    load();
  }

  const current = state();
  return {
    ...current,
    retry: () => {
      databasesCache.delete(slug);
      started.set("");
    },
  };
};

/** `d1` view body: the single declared database, editor, and empty states. */
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
  // Remount the whole editor when the app changes, so table and schema
  // state never leaks across apps.
  const editorKey = `${slug}`;

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

  // At most one database per app (deploy validation refuses a second), so
  // the single binding is used directly: no picker, no list.
  const binding = databases.value.databases[0]?.binding;

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

  // No wrapping card: the tab row + table rail + grid sit directly on the
  // main panel, which is itself the surface. On wide screens the view
  // takes the panel's full height (main is 100dvh minus its padding and
  // title) and the grid scrolls inside it.
  return (
    <div class="flex min-w-0 flex-col gap-3 md:h-[calc(100dvh-124px)]">
      {canWrite ? null : (
        <div class="alert alert-info alert-soft min-w-0" role="note">
          <span class="min-w-0 break-words">
            D1 editing is owner-only. You can browse tables and rows, but only
            the app owner can insert, edit, or delete rows.
          </span>
        </div>
      )}
      <div class="flex min-h-0 min-w-0 flex-1">
        <D1DatabaseView
          key={editorKey}
          binding={binding}
          canWrite={canWrite}
          route={route}
          setRoute={setRoute}
          slug={slug}
        />
      </div>
    </div>
  );
}

/** The single database's Tables/Backups tabs. Keyed per app by the caller,
 * so the picked tab resets when the app changes. No wrapping card: the tab
 * row and panels sit directly on the main panel surface. */
function D1DatabaseView({
  binding,
  canWrite,
  route,
  setRoute,
  slug,
}: {
  binding: string;
  canWrite: boolean;
  route: Route;
  setRoute: (next: Partial<Route>) => void;
  slug: string;
}) {
  const tab = atom<"tables" | "backups">("tables");

  return (
    <div class="flex min-h-0 min-w-0 flex-1 flex-col">
      <div class="border-base-300 overflow-x-auto border-b px-4 pt-2">
        <div role="tablist" class="tabs tabs-border w-max flex-nowrap" aria-label="D1 views">
          <button
            type="button"
            role="tab"
            aria-selected={tab() === "tables" ? "true" : "false"}
            class={`tab whitespace-nowrap ${tab() === "tables" ? "tab-active" : ""}`}
            onclick={() => tab.set("tables")}
          >
            Tables
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab() === "backups" ? "true" : "false"}
            class={`tab whitespace-nowrap ${tab() === "backups" ? "tab-active" : ""}`}
            onclick={() => tab.set("backups")}
          >
            Backups
          </button>
        </div>
      </div>
      {tab() === "tables" ? (
        <D1Editor
          binding={binding}
          canWrite={canWrite}
          route={route}
          setRoute={setRoute}
          slug={slug}
        />
      ) : (
        <BackupsPanel binding={binding} canWrite={canWrite} slug={slug} />
      )}
    </div>
  );
}

/** Sidebar Data entry: a plain `D1` row. The app declares at most one
 * database and the view uses its binding directly, so there is no nested
 * list and no count badge. */
export function D1Nav({
  active,
  setRoute,
}: {
  /** Whether the D1 view is showing. */
  active: boolean;
  setRoute: (next: Partial<Route>) => void;
}) {
  return (
    <li class="min-w-0">
      <button
        type="button"
        class={active ? "menu-active min-w-0" : "min-w-0"}
        aria-current={active ? "page" : undefined}
        onclick={() => {
          setRoute({ view: "d1" });
        }}
      >
        <DatabaseIcon />
        <span class="min-w-0 flex-1 truncate">D1</span>
      </button>
    </li>
  );
}
