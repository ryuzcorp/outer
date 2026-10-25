// D1 editor wire types: the snake_case JSON of
// `/api/console/apps/{slug}/data/d1/…`. Ported from Noite's
// `lib/runner.ts` D1 shapes (noite@58a977a); redacted/locked/caps/rowAction
// are Noite-only and stay out — the console is owner-only in v1.

/** One grid cell: SQL NULL or its exact stored text. */
export type D1Cell = string | null;

/** Row identity: PK columns' values (every column when the table has no PK). */
export type D1Key = Record<string, D1Cell>;

export interface D1Column {
  name: string;
  /** Declared type; "" when the DDL has none. */
  type: string;
  not_null: boolean;
  /** DDL default expression text; null when none. */
  default_value: string | null;
  /** 0 = not part of the PK, else 1-based position. */
  pk: number;
}

export interface D1ForeignKey {
  from: string;
  table: string;
  to: string;
}

export interface D1Index {
  name: string;
  unique: boolean;
  columns: string[];
}

export interface D1TableSchema {
  binding: string;
  table: string;
  columns: D1Column[];
  foreign_keys: D1ForeignKey[];
  indexes: D1Index[];
  sql: string | null;
}

export interface D1TableInfo {
  name: string;
  row_count: number;
}

export interface D1Database {
  binding: string;
  database_name: string;
}

export type D1FilterOp =
  | "eq"
  | "neq"
  | "lt"
  | "lte"
  | "gt"
  | "gte"
  | "like"
  | "is_null"
  | "not_null";

export interface D1Filter {
  column: string;
  op?: D1FilterOp;
  /** Ignored for is_null/not_null; `like` takes the user's pattern with `%` wildcards as typed. */
  value: string;
}

export interface D1Sort {
  column: string;
  desc: boolean;
}

/** The grid's query (minus `table`, which travels in the path). */
export interface D1RowsQuery {
  filters: D1Filter[];
  page: number;
  page_size: number;
  search: string;
  /** null = primary key order (rowid when the table has no PK). */
  sort: D1Sort | null;
  table: string;
}

export interface D1Rows {
  table: string;
  /** Column order of `rows`. */
  columns: string[];
  rows: D1Cell[][];
  /** COUNT(*) under the same filters/search. */
  total: number;
  page: number;
  page_size: number;
}

export type D1WriteOp = "insert" | "update" | "delete";
