// R2 browser actions: one registered entry per query/mutation the R2 panel
// performs. Reads are S3 under `fleets/{slug}/r2/<bucket>/` with scoped
// creds; writes and deletes go through `celld r2 put` / `celld r2 delete`
// and are audited (client "Console").

import { request } from "./http.ts";
import type { ActionContext, ConsoleAction } from "./settings.ts";

export interface R2Bucket {
  binding: string;
  bucket: string;
}

export interface R2Folder {
  name: string;
  prefix: string;
}

export interface R2Object {
  key: string;
  name: string;
  size: number;
  lastModified: string;
  contentType: string | null;
  etag: string | null;
}

export interface R2Preview {
  bucket: string;
  prefix: string;
  folders: R2Folder[];
  objects: R2Object[];
  nextCursor: string | null;
}

export interface R2File {
  key: string;
  size: number;
  truncated: boolean;
  contentType: string | null;
  text: string | null;
}

export interface BucketsResult {
  buckets: R2Bucket[];
}

const slugSchema = { type: "string", minLength: 1, maxLength: 64 };

function qs(params: Record<string, string | undefined>): string {
  const q = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined) {
      q.set(key, value);
    }
  }
  const text = q.toString();
  return text ? `?${text}` : "";
}

/** Same-origin raw-download URL for one object (inline preview + download
 * link). Not an action: it names a GET the browser makes itself. */
export function r2RawUrl(slug: string, bucket: string, key: string): string {
  return `/api/console/apps/${encodeURIComponent(slug)}/data/r2/raw${qs({ bucket, key })}`;
}

const r2ListBuckets: ConsoleAction<{ slug: string }, BucketsResult> = {
  name: "r2_list_buckets",
  title: "List R2 buckets",
  description:
    "List the app's declared R2 bucket (binding + bucket name; at most one) from its built wrangler config. The console uses the single bucket directly; an empty list means the app declares no r2_buckets.",
  inputSchema: {
    type: "object",
    properties: { slug: slugSchema },
    required: ["slug"],
  },
  readOnly: true,
  consequential: false,
  untrusted: true,
  run: ({ slug }, _ctx: ActionContext | undefined, signal) =>
    request<BucketsResult>(
      "GET",
      `/api/console/apps/${encodeURIComponent(slug)}/data/r2/buckets`,
      undefined,
      signal,
    ),
};

const r2List: ConsoleAction<
  { slug: string; bucket: string; prefix?: string; cursor?: string },
  R2Preview
> = {
  name: "r2_list",
  title: "List R2 folder",
  description:
    "List one folder of one R2 bucket: subfolders plus the files in it (name, size, content type, etag). Pass cursor from the previous page's nextCursor to page; prefix is the folder path ending in '/' ('' for the root).",
  inputSchema: {
    type: "object",
    properties: {
      slug: slugSchema,
      bucket: { type: "string", minLength: 1 },
      prefix: { type: "string" },
      cursor: { type: "string" },
    },
    required: ["slug", "bucket"],
  },
  readOnly: true,
  consequential: false,
  untrusted: true,
  run: ({ slug, bucket, prefix, cursor }, _ctx: ActionContext | undefined, signal) =>
    request<R2Preview>(
      "GET",
      `/api/console/apps/${encodeURIComponent(slug)}/data/r2/list${qs({ bucket, prefix, cursor })}`,
      undefined,
      signal,
    ),
};

const r2File: ConsoleAction<{ slug: string; bucket: string; key: string }, R2File> = {
  name: "r2_file",
  title: "Preview R2 object",
  description:
    "Bounded text preview of one R2 object (up to 256 KiB; text is null when the body is not UTF-8). Use for the detail panel; download the raw URL for whole bodies.",
  inputSchema: {
    type: "object",
    properties: {
      slug: slugSchema,
      bucket: { type: "string", minLength: 1 },
      key: { type: "string", minLength: 1 },
    },
    required: ["slug", "bucket", "key"],
  },
  readOnly: true,
  consequential: false,
  untrusted: true,
  run: ({ slug, bucket, key }, _ctx: ActionContext | undefined, signal) =>
    request<R2File>(
      "GET",
      `/api/console/apps/${encodeURIComponent(slug)}/data/r2/file${qs({ bucket, key })}`,
      undefined,
      signal,
    ),
};

const r2Delete: ConsoleAction<{ slug: string; bucket: string; keys: string[] }, { deleted: number }> = {
  name: "r2_delete",
  title: "Delete R2 objects",
  description:
    "Delete 1-100 objects from one R2 bucket by key. Irreversible; the panel confirms in a modal first. Audited as r2_delete.",
  inputSchema: {
    type: "object",
    properties: {
      slug: slugSchema,
      bucket: { type: "string", minLength: 1 },
      keys: { type: "array", items: { type: "string" }, minItems: 1, maxItems: 100 },
    },
    required: ["slug", "bucket", "keys"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run: ({ slug, bucket, keys }, _ctx: ActionContext | undefined, signal) =>
    request<{ deleted: number }>(
      "POST",
      `/api/console/apps/${encodeURIComponent(slug)}/data/r2/delete`,
      { bucket, keys },
      signal,
    ),
};

export interface R2UploadInput {
  slug: string;
  bucket: string;
  key: string;
  /** The bytes: a File from an <input> or a Blob (e.g. an empty Blob for a folder marker). Not JSON — carried as the request body. */
  file: Blob;
  contentType?: string;
}

const r2Upload: ConsoleAction<R2UploadInput, { key: string; size: number }> = {
  name: "r2_upload",
  title: "Upload R2 object",
  description:
    "Upload one object (or an empty body with a trailing-slash key for a folder marker) to one R2 bucket, up to 64 MiB. The stored record is what a Worker's put() writes. Audited as r2_put. The file bytes ride outside the JSON schema — pass them as `file`.",
  inputSchema: {
    type: "object",
    properties: {
      slug: slugSchema,
      bucket: { type: "string", minLength: 1 },
      key: { type: "string", minLength: 1 },
      contentType: { type: "string" },
    },
    required: ["slug", "bucket", "key"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run: async ({ slug, bucket, key, file, contentType }, _ctx: ActionContext | undefined, signal) => {
    const res = await fetch(
      `/api/console/apps/${encodeURIComponent(slug)}/data/r2/object${qs({ bucket, key })}`,
      {
        method: "PUT",
        headers: { "content-type": contentType ?? "application/octet-stream" },
        body: file,
        signal,
      },
    );
    if (res.status === 401) {
      globalThis.location.assign("/console/sign-in");
      throw new Error("signed out");
    }
    if (!res.ok) {
      let message = `request failed: ${res.status}`;
      try {
        const body = (await res.json()) as { user_message?: unknown };
        if (typeof body.user_message === "string" && body.user_message) {
          message = body.user_message;
        }
      } catch {
        // Fall through with the status text.
      }
      throw new Error(message);
    }
    return (await res.json()) as { key: string; size: number };
  },
};


export const actions: ConsoleAction<any, any>[] = [
  r2ListBuckets,
  r2List,
  r2File,
  r2Delete,
  r2Upload,
];

export const r2Actions = {
  listBuckets: r2ListBuckets,
  list: r2List,
  file: r2File,
  deleteObjects: r2Delete,
  upload: r2Upload,
};
