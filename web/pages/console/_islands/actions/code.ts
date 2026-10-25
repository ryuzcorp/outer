// Console actions for the Code view: read-only queries against the app's
// repo at a chosen version. Every entry goes through the console API (the
// same JSON the components could fetch), so the later ⌘K palette and the
// WebMCP layer can invoke them too. Outputs carry app-generated text, hence
// `untrusted: true` on all of them.
import { api } from "../api.ts";
import type { ActionContext } from "./settings.ts";

export interface ConsoleAction<I, O> {
  name: string;
  title: string;
  description: string;
  inputSchema: object;
  readOnly: boolean;
  consequential: boolean;
  untrusted: boolean;
  run(input: I, ctx?: ActionContext, signal?: AbortSignal): Promise<O>;
}

export interface TreeFile {
  path: string;
  size: number;
}

export interface TreeResult {
  sha: string;
  live_sha: string | null;
  files: TreeFile[];
  truncated: boolean;
}

export interface FileResult {
  path: string;
  sha: string;
  kind: "text" | "binary" | "too_large";
  text?: string;
  truncated?: boolean;
  total_lines?: number;
  size: number;
  live_sha: string | null;
}

export interface HistoryCommit {
  sha: string;
  author: string;
  email: string;
  at: number;
  message: string;
}

export interface HistoryResult {
  head: string | null;
  live_sha: string | null;
  commits: HistoryCommit[];
  next_skip: number;
}

export interface DiffFile {
  path: string;
  status: string;
  added: number;
  removed: number;
  binary: boolean;
}

export interface DiffResult {
  commit: HistoryCommit;
  parent: string | null;
  files: DiffFile[];
  patch: string;
  truncated: boolean;
}

export interface CompareResult {
  from: string;
  to: string;
  files: DiffFile[];
  patch: string;
  truncated: boolean;
}

function get<T>(base: string, params: Record<string, string | undefined>): Promise<T> {
  const q = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== "") {
      q.set(key, value);
    }
  }
  const suffix = q.toString();
  return api<T>(suffix ? `${base}?${suffix}` : base);
}

const slugSchema = { type: "string", description: "The app slug." };
const shaSchema = {
  type: "string",
  description: "A commit sha (default: the live sha, the latest successful deploy).",
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  {
    name: "code_list_files",
    title: "List files at a version",
    description:
      "List the app repo's files at a version (default: live). Returns the resolved sha, the live sha, and path/size pairs. Use prefix to scope under one directory.",
    inputSchema: {
      type: "object",
      properties: { slug: slugSchema, sha: shaSchema, prefix: { type: "string" } },
      required: ["slug"],
      additionalProperties: false,
    },
    readOnly: true,
    consequential: false,
    untrusted: true,
    run(input: { slug: string; sha?: string; prefix?: string }): Promise<TreeResult> {
      const base = `/api/console/apps/${encodeURIComponent(input.slug)}/code/tree`;
      return get<TreeResult>(base, { sha: input.sha, prefix: input.prefix });
    },
  },
  {
    name: "code_read_file",
    title: "Read a file at a version",
    description:
      "Read one repo file at a version (default: live). Text comes with line counts and a 64 KiB cap; binary and over-cap files return their kind and size only.",
    inputSchema: {
      type: "object",
      properties: { slug: slugSchema, path: { type: "string" }, sha: shaSchema },
      required: ["slug", "path"],
      additionalProperties: false,
    },
    readOnly: true,
    consequential: false,
    untrusted: true,
    run(input: { slug: string; path: string; sha?: string }): Promise<FileResult> {
      const base = `/api/console/apps/${encodeURIComponent(input.slug)}/code/file`;
      return get<FileResult>(base, { path: input.path, sha: input.sha });
    },
  },
  {
    name: "code_history",
    title: "List commits",
    description:
      "List main-branch commits newest first with author, time and message, plus the live sha. Page with skip (next_skip is the next cursor).",
    inputSchema: {
      type: "object",
      properties: {
        slug: slugSchema,
        path: { type: "string" },
        skip: { type: "string", description: "Commits to skip (next_skip from the previous page)." },
      },
      required: ["slug"],
      additionalProperties: false,
    },
    readOnly: true,
    consequential: false,
    untrusted: true,
    run(input: { slug: string; path?: string; skip?: string }): Promise<HistoryResult> {
      const base = `/api/console/apps/${encodeURIComponent(input.slug)}/code/history`;
      return get<HistoryResult>(base, { path: input.path, skip: input.skip });
    },
  },
  {
    name: "code_diff",
    title: "Show a commit's changes",
    description:
      "Show one commit's changed files (status plus added/removed line counts) and its unified patch vs its parent. Root commits diff against the empty tree.",
    inputSchema: {
      type: "object",
      properties: { slug: slugSchema, sha: { type: "string", description: "The commit sha." } },
      required: ["slug", "sha"],
      additionalProperties: false,
    },
    readOnly: true,
    consequential: false,
    untrusted: true,
    run(input: { slug: string; sha: string }): Promise<DiffResult> {
      const base = `/api/console/apps/${encodeURIComponent(input.slug)}/code/diff`;
      return get<DiffResult>(base, { sha: input.sha });
    },
  },
  {
    name: "code_compare",
    title: "Compare two versions",
    description:
      "Compare two versions (from, to): changed files with counts plus the unified patch. Pass to as the live sha for the 'compare with live' view.",
    inputSchema: {
      type: "object",
      properties: {
        slug: slugSchema,
        from: { type: "string" },
        to: { type: "string" },
      },
      required: ["slug", "from", "to"],
      additionalProperties: false,
    },
    readOnly: true,
    consequential: false,
    untrusted: true,
    run(input: { slug: string; from: string; to: string }): Promise<CompareResult> {
      const base = `/api/console/apps/${encodeURIComponent(input.slug)}/code/compare`;
      return get<CompareResult>(base, { from: input.from, to: input.to });
    },
  },
];
