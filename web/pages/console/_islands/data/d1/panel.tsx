// The D1 table editor (Supabase-style): table rail, toolbar, server-paged
// grid, Definition view, row editor and toasts. Every query the grid runs
// comes from validated URL params (see ./state) riding the console route, so
// a view is deep-linkable and a write can invalidate exactly what it
// changed. Ported from Noite's `lib/storage/d1/panel.tsx` (noite@58a977a);
// reads run through the registered actions, and the Noite-only resource
// layer, redaction and capability gates are gone.

import { atom } from "ilha";
import type { View } from "ilha";

import { d1run } from "../../actions/data-d1.ts";
import type {
  D1RowsInput,
  D1SchemaInput,
  D1TablesInput,
} from "../../actions/data-d1.ts";
import type { Route } from "../../common.tsx";
import { Toaster, useToasts } from "../shared.tsx";
import { TableDefinition } from "./definition.tsx";
import { RowsGrid } from "./grid.tsx";
import { RowEditor } from "./row-editor.tsx";
import { TableSelect, TableSidebar } from "./sidebar.tsx";
import { TableToolbar } from "./toolbar.tsx";
import type {
  D1Cell,
  D1Column,
  D1Filter,
  D1Rows,
  D1RowsQuery,
  D1TableInfo,
  D1TableSchema,
} from "./types.ts";
import {
  decodeFilters,
  decodeSort,
  DEFAULT_PAGE_SIZE,
  encodeFilters,
  encodeSort,
  toPageIndex,
  toPageSize,
  toView,
  validFilters,
  validSort,
  VIEW_LABELS,
  VIEWS,
} from "./state.ts";
import type { D1View } from "./state.ts";

/** The editor's URL-backed state, read once per render. */
interface UrlState {
  filters: D1Filter[];
  page: number;
  pageSize: number;
  search: string;
  sort: D1RowsQuery["sort"];
  view: D1View;
}

/** Tables state for one mounted binding. The editor remounts per binding +
 * generation, so the fetch fires once per mount. */
type TablesState =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; value: D1TableInfo[] };

/** Schema state for one mounted table. */
type SchemaState =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; value: D1TableSchema };

/** Rows state for one mounted query. The panel (not the grid) owns the
 * fetch so it fires in parallel with the schema fetch. */
type RowsFetchState =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; value: D1Rows };

const ViewSwitch = ({
  onPick,
  view,
}: {
  onPick: (view: D1View) => void;
  view: D1View;
}) => (
  <div class="join" role="group" aria-label="View">
    {VIEWS.map((candidate) => (
      <button
        key={candidate}
        type="button"
        class={`btn btn-sm join-item ${view === candidate ? "btn-active" : ""}`}
        aria-pressed={view === candidate ? "true" : "false"}
        onclick={() => {
          onPick(candidate);
        }}
      >
        {VIEW_LABELS[candidate]}
      </button>
    ))}
  </div>
);

/** One table's workspace: schema, toolbar, grid/definition, pager, editor. */
const TableWorkspace = ({
  binding,
  canWrite,
  generation,
  nav,
  onChanged,
  onToast,
  route,
  setRoute,
  slug,
  table,
}: {
  binding: string;
  /** False for collaborators: toolbar and grid render read-only. */
  canWrite: boolean;
  generation: number;
  nav: {
    openForeignKey: (target: {
      column: string;
      table: string;
      value: D1Cell;
    }) => void;
  };
  onChanged: () => void;
  onToast: (text: string) => void;
  route: Route;
  setRoute: (next: Partial<Route>) => void;
  slug: string;
  table: string;
}) => {
  const schemaState = atom<SchemaState>({ kind: "loading" });
  const rowsState = atom<RowsFetchState>({ kind: "loading" });
  const editor = atom<null | { row: Record<string, D1Cell> | null }>(null);
  const startedSchema = atom("");
  const startedRows = atom("");

  const schemaKey = `${binding}:${table}:${generation}`;
  if (startedSchema() !== schemaKey) {
    startedSchema.set(schemaKey);
    const key = schemaKey;
    const input: D1SchemaInput = { slug, binding, table };
    d1run<D1SchemaInput, D1TableSchema>("d1_schema", input).then(
      (value) => {
        if (startedSchema() === key) {
          schemaState.set({ kind: "ready", value });
        }
      },
      (error: unknown) => {
        if (startedSchema() === key) {
          schemaState.set({
            kind: "error",
            message: error instanceof Error ? error.message : String(error),
          });
        }
      },
    );
  }

  // Rows fire alongside the schema fetch with the raw URL params, then
  // re-fire with schema-validated params only when validation would change
  // them — a stale URL naming a dropped column heals once the schema lands.
  const rawQuery: D1RowsQuery = {
    filters: decodeFilters(route.f),
    page: toPageIndex(route.page),
    page_size: toPageSize(route.size),
    search: route.q,
    sort: decodeSort(route.sort),
    table,
  };
  const schemaNow = schemaState();
  const validatedQuery: D1RowsQuery =
    schemaNow.kind === "ready"
      ? {
          ...rawQuery,
          sort: validSort(rawQuery.sort, schemaNow.value),
          filters: validFilters(rawQuery.filters, schemaNow.value),
        }
      : rawQuery;
  const rowsKey = `${schemaKey}:${JSON.stringify(validatedQuery)}`;
  if (startedRows() !== rowsKey) {
    startedRows.set(rowsKey);
    const key = rowsKey;
    const input: D1RowsInput = { slug, binding, ...validatedQuery };
    d1run<D1RowsInput, D1Rows>("d1_rows", input).then(
      (value) => {
        if (startedRows() === key) {
          rowsState.set({ kind: "ready", value });
        }
      },
      (error: unknown) => {
        if (startedRows() === key) {
          rowsState.set({
            kind: "error",
            message: error instanceof Error ? error.message : String(error),
          });
        }
      },
    );
  }

  const current = schemaState();

  if (current.kind === "error") {
    return (
      <div class="flex flex-col items-start gap-2 p-4">
        <p class="text-error m-0 text-sm">{current.message}</p>
        <button
          type="button"
          class="btn btn-sm"
          onclick={() => {
            schemaState.set({ kind: "loading" });
            startedSchema.set("");
          }}
        >
          Retry
        </button>
      </div>
    );
  }

  if (current.kind === "loading") {
    return (
      <div class="flex min-h-0 flex-1 flex-col gap-2 p-4">
        <span class="skeleton h-8 w-full" />
        <span class="skeleton h-6 w-full" />
        <span class="skeleton h-6 w-full" />
      </div>
    );
  }

  const schema = current.value;
  const urlState: UrlState = {
    filters: decodeFilters(route.f),
    page: toPageIndex(route.page),
    pageSize: toPageSize(route.size),
    search: route.q,
    sort: decodeSort(route.sort),
    view: toView(route.dview),
  };
  // Same validation the fetch above applies once the schema is ready: the
  // grid's key and sort marks track exactly what `validatedQuery` carries.
  const query: D1RowsQuery = {
    ...rawQuery,
    sort: validSort(rawQuery.sort, schema),
    filters: validFilters(rawQuery.filters, schema),
  };

  const setFilters = (next: D1Filter[]) => {
    setRoute({ f: encodeFilters(next), page: "" });
  };

  const viewSlot = (
    <ViewSwitch
      onPick={(view) => {
        setRoute({ dview: view === "data" ? "" : view });
      }}
      view={urlState.view}
    />
  );
  const open = editor();
  const openKey =
    open === null
      ? "closed"
      : open.row === null
        ? "insert"
        : `edit:${JSON.stringify(open.row)}`;

  const cycleSort = (column: string) => {
    const active = query.sort;
    if (active === null || active.column !== column) {
      setRoute({ sort: encodeSort({ column, desc: false }), page: "" });
      return;
    }
    setRoute({
      sort: encodeSort(active.desc ? null : { column, desc: true }),
      page: "",
    });
  };

  const rows = rowsState();

  return (
    <div class="flex min-h-0 flex-1 flex-col">
      {urlState.view === "data" ? (
        <TableToolbar
          applied={query}
          canWrite={canWrite}
          columns={schema.columns}
          onApplyFilters={setFilters}
          onApplySort={(next) => {
            setRoute({ sort: encodeSort(next), page: "" });
          }}
          onInsert={() => {
            editor.set({ row: null });
          }}
          onRefresh={onChanged}
          onSearch={(needle) => {
            setRoute({ q: needle, page: "" });
          }}
        />
      ) : null}
      {urlState.view === "definition" ? (
        <TableDefinition schema={schema} />
      ) : rows.kind === "ready" ? (
        <RowsGrid
          key={JSON.stringify({ ...query, generation })}
          binding={binding}
          canWrite={canWrite}
          data={rows.value}
          onChanged={onChanged}
          onOpenRow={(row) => {
            editor.set({ row });
          }}
          onPage={(next) => {
            setRoute({ page: next === 0 ? "" : String(next) });
          }}
          onSize={(next) => {
            setRoute({
              size: next === DEFAULT_PAGE_SIZE ? "" : String(next),
              page: "",
            });
          }}
          onSort={cycleSort}
          onToast={onToast}
          query={query}
          schema={schema}
          slug={slug}
          viewSlot={viewSlot as View}
        />
      ) : rows.kind === "error" ? (
        <div class="flex flex-col items-start gap-2 p-4">
          <p class="text-error m-0 text-sm">{rows.message}</p>
          <button
            type="button"
            class="btn btn-sm"
            onclick={() => {
              rowsState.set({ kind: "loading" });
              startedRows.set("");
            }}
          >
            Retry
          </button>
        </div>
      ) : (
        <div class="flex flex-col gap-2 p-4">
          <span class="skeleton h-8 w-full" />
          <span class="skeleton h-6 w-full" />
          <span class="skeleton h-6 w-full" />
        </div>
      )}
      {urlState.view === "definition" ? (
        <div class="border-base-300 flex items-center justify-end border-t px-3 py-2">
          {viewSlot}
        </div>
      ) : null}
      {open === null ? null : (
        <RowEditor
          key={openKey}
          binding={binding}
          onClose={() => {
            editor.set(null);
          }}
          onOpenFk={nav.openForeignKey}
          onSaved={onChanged}
          onToast={onToast}
          row={open.row}
          schema={schema}
          slug={slug}
        />
      )}
    </div>
  );
};

export const D1Editor = ({
  binding,
  canWrite,
  route,
  setRoute,
  slug,
}: {
  binding: string;
  /** False for collaborators: tables and rows stay browsable, writes hide. */
  canWrite: boolean;
  route: Route;
  setRoute: (next: Partial<Route>) => void;
  slug: string;
}) => {
  const state = atom<TablesState>({ kind: "loading" });
  const generation = atom(0);
  const started = atom("");

  const tablesKey = `${binding}:${generation()}`;
  if (started() !== tablesKey) {
    started.set(tablesKey);
    const input: D1TablesInput = { slug, binding };
    d1run<D1TablesInput, { tables: D1TableInfo[] }>("d1_tables", input).then(
      (value) => {
        state.set({ kind: "ready", value: value.tables });
      },
      (error: unknown) => {
        state.set({
          kind: "error",
          message: error instanceof Error ? error.message : String(error),
        });
      },
    );
  }

  const { notify: onToast, toasts } = useToasts();
  const current = state();

  if (current.kind === "error") {
    return (
      <div class="flex flex-col items-start gap-2 p-4">
        <p class="text-error m-0 text-sm">{current.message}</p>
        <button
          type="button"
          class="btn btn-sm"
          onclick={() => {
            state.set({ kind: "loading" });
            started.set("");
          }}
        >
          Retry
        </button>
      </div>
    );
  }

  if (current.kind === "loading") {
    return (
      <div class="flex min-h-0 flex-1 flex-col gap-2 p-4">
        <span class="skeleton h-8 w-full" />
        <span class="skeleton h-32 w-full" />
      </div>
    );
  }

  const tables = current.value;
  const selected =
    tables.find((candidate) => candidate.name === route.table) ?? tables[0];

  if (selected === undefined) {
    return (
      <p class="m-0 p-4 text-sm opacity-60">This database has no tables yet.</p>
    );
  }

  const switchTable = (next: string) => {
    setRoute({ table: next, f: "", sort: "", q: "", page: "" });
  };

  const nav = {
    /** Jump to the table a foreign key points at, filtered to that row. */
    openForeignKey: (target: {
      column: string;
      table: string;
      value: D1Cell;
    }) => {
      setRoute({
        table: target.table,
        f:
          target.value === null
            ? ""
            : encodeFilters([
                { column: target.column, op: "eq", value: target.value },
              ]),
        sort: "",
        q: "",
        page: "",
        dview: "",
      });
    },
  };

  return (
    <div class="flex h-full min-h-0 w-full overflow-hidden">
      <TableSidebar
        binding={binding}
        current={selected.name}
        onSelect={switchTable}
        tables={tables}
      />
      <div class="flex min-w-0 flex-1 flex-col">
        <TableSelect
          binding={binding}
          current={selected.name}
          onSelect={switchTable}
          tables={tables}
        />
        <div class="border-base-300 overflow-x-auto border-b">
          <div role="tablist" class="tabs tabs-border w-max flex-nowrap">
            <button
              type="button"
              role="tab"
              aria-selected="true"
              class="tab tab-active gap-2 whitespace-nowrap"
            >
              <span class="truncate">{selected.name}</span>
              <span class="badge badge-sm tabular-nums">
                {selected.row_count}
              </span>
            </button>
          </div>
        </div>
        <TableWorkspace
          key={`${selected.name}:${generation()}`}
          binding={binding}
          canWrite={canWrite}
          generation={generation()}
          nav={nav}
          onChanged={() => {
            generation.set(generation() + 1);
          }}
          onToast={onToast}
          route={route}
          setRoute={setRoute}
          slug={slug}
          table={selected.name}
        />
      </div>
      <Toaster toasts={toasts()} />
    </div>
  );
};
