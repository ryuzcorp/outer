// D1 console actions. Every query and mutation the D1 editor performs
// goes through one entry here, shaped as the console's `ConsoleAction`
// contract (see CONSOLE-WAVE2.md: the ⌘K palette and WebMCP layer import
// every `actions/*.ts` registry). Components call `run`; they never fetch
// the D1 API directly.
//
// Reads are `readOnly`; writes, manual backups and restores are
// `consequential` (they change app data and audit to the action log). Row
// content is app/user data, so reads of rows are `untrusted`.

import type { ActionContext } from "./settings.ts";
import type {
  D1Database,
  D1Filter,
  D1Rows,
  D1Sort,
  D1TableInfo,
  D1TableSchema,
  D1WriteOp,
  D1Key,
  D1Cell,
} from "../data/d1/types.ts";

export interface ConsoleAction<I, O> {
  /** snake_case, WebMCP-valid: [A-Za-z0-9_.-]{1,128}. */
  name: string;
  /** Short human label. */
  title: string;
  /** Agent-facing: what it does, when to use it, what it returns. */
  description: string;
  /** JSON Schema for I. */
  inputSchema: object;
  /** WebMCP readOnlyHint. */
  readOnly: boolean;
   /** WebMCP consequentialHint (destructive/irreversible). */
   consequential: boolean;
   /** WebMCP untrustedContentHint (output includes app/user-generated text). */
   untrusted: boolean;
  run(input: I, ctx?: ActionContext, signal?: AbortSignal): Promise<O>;
}

const base = (slug: string, binding: string, rest: string): string =>
  `/api/console/apps/${encodeURIComponent(slug)}/data/d1/${encodeURIComponent(binding)}${rest}`;

/** Look up one registered action by name and call its `run` with a typed
 * payload. Throws when the name is not registered (a programming error,
 * not a user error). */
export function d1run<I, O>(
  name: string,
  input: I,
  ctx?: ActionContext,
  signal?: AbortSignal,
): Promise<O> {
  const action = actions.find((candidate) => candidate.name === name) as
    | ConsoleAction<I, O>
    | undefined;
  if (action === undefined) {
    throw new Error(`unknown action ${name}`);
  }
  return action.run(input, ctx, signal);
}
/** GET with the console's 401 handling (missing/expired session signs the
 * human through the Rust sign-in flow) and the D1 error envelope
 * (`{error, message, user_message}` — the UI shows `user_message`). */
async function d1get<T>(path: string, signal?: AbortSignal): Promise<T> {
  const res = await fetch(path, { headers: { accept: "application/json" }, signal });
  if (res.status === 401) {
    globalThis.location.assign("/console/sign-in");
    throw new Error("signed out");
  }
  const body = (await res.json()) as {
    error?: string;
    message?: string;
    user_message?: string;
  };
  if (!res.ok) {
    throw new Error(
      body.user_message ?? body.message ?? `request failed: ${res.status}`,
    );
  }
  return body as T;
}

/** POST twin of `d1get` (same-origin JSON, bounded bodies). */
async function d1post<T>(path: string, payload: unknown, signal?: AbortSignal): Promise<T> {
  const res = await fetch(path, {
    method: "POST",
    headers: { accept: "application/json", "content-type": "application/json" },
    body: JSON.stringify(payload),
    signal,
  });
  if (res.status === 401) {
    globalThis.location.assign("/console/sign-in");
    throw new Error("signed out");
  }
  const body = (await res.json()) as {
    error?: string;
    message?: string;
    user_message?: string;
  };
  if (!res.ok) {
    throw new Error(
      body.user_message ?? body.message ?? `request failed: ${res.status}`,
    );
  }
  return body as T;
}

export interface D1DatabasesInput {
  slug: string;
}

export interface D1DatabasesOutput {
  databases: D1Database[];
}

export interface D1TablesInput {
  slug: string;
  binding: string;
}

export interface D1TablesOutput {
  binding: string;
  database: string;
  tables: D1TableInfo[];
}

export interface D1SchemaInput {
  slug: string;
  binding: string;
  table: string;
}

export interface D1RowsInput {
  slug: string;
  binding: string;
  table: string;
  filters: D1Filter[];
  page: number;
  page_size: number;
  search: string;
  sort: D1Sort | null;
}

export interface D1WriteInput {
  slug: string;
  binding: string;
  table: string;
  op: D1WriteOp;
  key: D1Key;
  values: Record<string, D1Cell>;
}

export interface D1DeleteRowsInput {
  slug: string;
  binding: string;
  table: string;
  keys: D1Key[];
}

export interface D1Backup {
  id: string;
  binding: string;
  created_at: number;
  size_bytes: number;
}

export interface D1BackupsInput {
  slug: string;
  binding: string;
}

export interface D1BackupsOutput {
  binding: string;
  backups: D1Backup[];
}

export interface D1BackupNowInput {
  slug: string;
  binding: string;
}

export interface D1BackupNowOutput {
  backup: D1Backup;
}

export interface D1BackupRestoreInput {
  slug: string;
  binding: string;
  backup_id: string;
  confirm: string;
}

export interface D1BackupRestoreOutput {
  backup: D1Backup;
  restore: {
    tables_dropped: number;
    statements_applied: number;
    bytes_applied: number;
  };
}
const slugSchema = { type: "string", minLength: 1, maxLength: 128 };
const bindingSchema = { type: "string", minLength: 1, maxLength: 128 };
const tableSchema = { type: "string", minLength: 1, maxLength: 128 };

const filterSchema = {
  type: "object",
  required: ["column"],
  properties: {
    column: { type: "string", maxLength: 128 },
    op: {
      type: "string",
      enum: ["eq", "neq", "lt", "lte", "gt", "gte", "like", "is_null", "not_null"],
    },
    value: { type: "string", maxLength: 4096 },
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  {
    name: "d1_databases",
    title: "List D1 databases",
    description:
      "List the app's declared D1 database (binding + database name; at most one). Pass its `binding` to the other d1_* actions. Returns {databases}.",
    inputSchema: {
      type: "object",
      required: ["slug"],
      properties: { slug: slugSchema },
    },
    readOnly: true,
    consequential: false,
    untrusted: false,
    run(input: D1DatabasesInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<D1DatabasesOutput> {
      return d1get<D1DatabasesOutput>(
        `/api/console/apps/${encodeURIComponent(input.slug)}/data/d1/databases`,
        signal,
      );
    },
  },
  {
    name: "d1_tables",
    title: "List D1 tables",
    description:
      "List one D1 database's user tables with row counts (internals hidden). Returns {binding, database, tables:[{name, row_count}]}.",
    inputSchema: {
      type: "object",
      required: ["slug", "binding"],
      properties: { slug: slugSchema, binding: bindingSchema },
    },
    readOnly: true,
    consequential: false,
    untrusted: false,
    run(input: D1TablesInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<D1TablesOutput> {
      return d1get<D1TablesOutput>(`${base(input.slug, input.binding, "/tables")}`, signal);
    },
  },
  {
    name: "d1_schema",
    title: "Describe a D1 table",
    description:
      "Describe one table: columns (name, type, nullability, default, pk position), foreign keys, indexes and the CREATE statement. Returns the schema the row editor and grid need.",
    inputSchema: {
      type: "object",
      required: ["slug", "binding", "table"],
      properties: { slug: slugSchema, binding: bindingSchema, table: tableSchema },
    },
    readOnly: true,
    consequential: false,
    untrusted: false,
    run(input: D1SchemaInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<D1TableSchema> {
      return d1get<D1TableSchema>(
        `${base(input.slug, input.binding, `/tables/${encodeURIComponent(input.table)}/schema`)}`,
        signal,
      );
    },
  },
  {
    name: "d1_rows",
    title: "Read D1 rows",
    description:
      "Read one server-side page of a table (paged, sorted, filtered, searched). page is 0-based; page_size clamps to 1..100; at most 10 filters. Returns {table, columns, rows, total, page, page_size}; cells are strings or null.",
    inputSchema: {
      type: "object",
      required: ["slug", "binding", "table"],
      properties: {
        slug: slugSchema,
        binding: bindingSchema,
        table: tableSchema,
        filters: { type: "array", maxItems: 10, items: filterSchema },
        page: { type: "integer", minimum: 0 },
        page_size: { type: "integer", minimum: 1, maximum: 100 },
        search: { type: "string", maxLength: 1024 },
        sort: {
          type: ["object", "null"],
          required: ["column"],
          properties: {
            column: { type: "string", maxLength: 128 },
            desc: { type: "boolean" },
          },
        },
      },
    },
    readOnly: true,
    consequential: false,
    untrusted: true,
    run(input: D1RowsInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<D1Rows> {
      const { slug, binding, table, ...query } = input;
      return d1post<D1Rows>(
        `${base(slug, binding, `/tables/${encodeURIComponent(table)}/rows`)}`,
        query,
        signal,
      );
    },
  },
  {
    name: "d1_write",
    title: "Write a D1 row",
    description:
      "Insert, update or delete one row with a curated statement (no freeform SQL). update/delete identify the row by its primary key (`key`); insert omits `key`. Values are strings or null (null = SQL NULL). Audited to the action log as Console.",
    inputSchema: {
      type: "object",
      required: ["slug", "binding", "table", "op"],
      properties: {
        slug: slugSchema,
        binding: bindingSchema,
        table: tableSchema,
        op: { type: "string", enum: ["insert", "update", "delete"] },
        key: { type: "object" },
        values: { type: "object" },
      },
    },
    readOnly: false,
    consequential: true,
    untrusted: false,
    run(
      input: D1WriteInput,
      _ctx?: ActionContext,
      signal?: AbortSignal,
    ): Promise<{ ok: true }> {
      const { slug, binding, table, ...body } = input;
      return d1post<{ ok: true }>(
        `${base(slug, binding, `/tables/${encodeURIComponent(table)}/write`)}`,
        body,
        signal,
      );
    },
  },
  {
    name: "d1_delete_rows",
    title: "Bulk-delete D1 rows",
    description:
      "Delete 1..100 rows by primary key in one atomic transaction (all-or-nothing). Returns {deleted}. Audited to the action log as Console.",
    inputSchema: {
      type: "object",
      required: ["slug", "binding", "table", "keys"],
      properties: {
        slug: slugSchema,
        binding: bindingSchema,
        table: tableSchema,
        keys: { type: "array", minItems: 1, maxItems: 100, items: { type: "object" } },
      },
    },
    readOnly: false,
    consequential: true,
    untrusted: false,
    run(
      input: D1DeleteRowsInput,
      _ctx?: ActionContext,
      signal?: AbortSignal,
    ): Promise<{ deleted: number }> {
      const { slug, binding, table, ...body } = input;
      return d1post<{ deleted: number }>(
        `${base(slug, binding, `/tables/${encodeURIComponent(table)}/delete-rows`)}`,
        body,
        signal,
      );
    },
  },
  {
    name: "d1_backups",
    title: "List D1 backups",
    description:
      "List a D1 database's stored backups: id, when each dump was taken and its size. Daily dumps are automatic for active apps (a request or successful deploy in the prior 24 h; newest 3 kept, Pro 30). Returns {binding, backups}.",
    inputSchema: {
      type: "object",
      required: ["slug", "binding"],
      properties: { slug: slugSchema, binding: bindingSchema },
    },
    readOnly: true,
    consequential: false,
    untrusted: false,
    run(input: D1BackupsInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<D1BackupsOutput> {
      return d1get<D1BackupsOutput>(`${base(input.slug, input.binding, "/backups")}`, signal);
    },
  },
  {
    name: "d1_backup_now",
    title: "Back up a D1 database now",
    description:
      "Take a D1 backup now and store it (pruning past retention on the way out). Returns {backup}. Audited to the action log as Console.",
    inputSchema: {
      type: "object",
      required: ["slug", "binding"],
      properties: { slug: slugSchema, binding: bindingSchema },
    },
    readOnly: false,
    consequential: true,
    untrusted: false,
    run(
      input: D1BackupNowInput,
      _ctx?: ActionContext,
      signal?: AbortSignal,
    ): Promise<D1BackupNowOutput> {
      return d1post<D1BackupNowOutput>(
        `${base(input.slug, input.binding, "/backups")}`,
        {},
        signal,
      );
    },
  },
  {
    name: "d1_backup_restore",
    title: "Restore a D1 backup",
    description:
      "Restore a D1 database from a backup id (pass confirm equal to backup_id). Takes a fresh backup first, then replaces the live tables. Returns {backup, restore}. Audited to the action log as Console.",
    inputSchema: {
      type: "object",
      required: ["slug", "binding", "backup_id", "confirm"],
      properties: {
        slug: slugSchema,
        binding: bindingSchema,
        backup_id: { type: "string", minLength: 1, maxLength: 128 },
        confirm: { type: "string", minLength: 1, maxLength: 128 },
      },
    },
    readOnly: false,
    consequential: true,
    untrusted: false,
    run(
      input: D1BackupRestoreInput,
      _ctx?: ActionContext,
      signal?: AbortSignal,
    ): Promise<D1BackupRestoreOutput> {
      const { slug, binding, backup_id, ...body } = input;
      return d1post<D1BackupRestoreOutput>(
        `${base(slug, binding, `/backups/${encodeURIComponent(backup_id)}/restore`)}`,
        body,
        signal,
      );
    },
  },
];
