// Admin code inspection: the same five read-only Code queries as
// `code.ts`, answered by the admin-guarded
// `/api/console/admin/apps/:slug/code/…` routes (spec §15.3) instead of
// the role-scoped ones. Used only by the Code view in admin-inspection
// mode from the moderation queue: the admin may have no owner or
// collaborator role on the app, so the normal routes 404. Every read is
// audited server-side. Deliberately NOT registered in `index.ts`: the
// palette and WebMCP never offer them; the view imports them directly.
import { api } from "../api.ts";
import type {
  CompareResult,
  DiffResult,
  FileResult,
  HistoryResult,
  TreeResult,
} from "./code.ts";

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

const baseFor = (slug: string, rest: string): string =>
  `/api/console/admin/apps/${encodeURIComponent(slug)}/code/${rest}`;

export const adminCodeActions = {
  tree(input: { slug: string; sha?: string; prefix?: string }): Promise<TreeResult> {
    return get<TreeResult>(baseFor(input.slug, "tree"), {
      sha: input.sha,
      prefix: input.prefix,
    });
  },
  file(input: { slug: string; path: string; sha?: string }): Promise<FileResult> {
    return get<FileResult>(baseFor(input.slug, "file"), {
      path: input.path,
      sha: input.sha,
    });
  },
  history(input: { slug: string; path?: string; skip?: string }): Promise<HistoryResult> {
    return get<HistoryResult>(baseFor(input.slug, "history"), {
      path: input.path,
      skip: input.skip,
    });
  },
  diff(input: { slug: string; sha: string }): Promise<DiffResult> {
    return get<DiffResult>(baseFor(input.slug, "diff"), { sha: input.sha });
  },
  compare(input: { slug: string; from: string; to: string }): Promise<CompareResult> {
    return get<CompareResult>(baseFor(input.slug, "compare"), {
      from: input.from,
      to: input.to,
    });
  },
};
