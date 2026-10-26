// Admin All Apps view (admin-all-apps branch): the directory of every app
// on Outer. Rendered inside the existing Admin account view; it fetches its
// own data so the shell's account loaders stay untouched.
//
// Reads go through GET /api/console/admin/apps (404 for non-admins). Opening
// a row navigates to the normal Console app views, where the server grants
// the admin owner-equivalent access after a fresh sign-in (step-up): a 401
// here sends the admin through the sign-in flow, exactly like a session
// expiry.
import { atom } from "ilha";

import { api, when } from "../api.ts";
import { adminAppsQuery, type AdminAppsParams } from "./admin-apps-query.ts";

export interface AdminAppDeploy {
  id: string;
  sha: string;
  status: string;
  created_at: number;
}

export interface AdminAppRow {
  slug: string;
  name: string | null;
  owner_email: string;
  plan: string;
  visibility: string;
  moderation_state: string;
  created_at: number;
  last_deploy: AdminAppDeploy | null;
}

export interface AdminAppsPage {
  apps: AdminAppRow[];
  next: string | null;
}

export function AdminAllAppsCard({ onOpenApp }: { onOpenApp: (slug: string) => void }) {
  const draft = atom("");
  const applied = atom({ q: "", state: "", plan: "" });
  const rows = atom<AdminAppRow[]>([]);
  const next = atom<string | null>(null);
  const loading = atom(true);
  const loadingMore = atom(false);
  const error = atom<string | null>(null);

  async function load(params: AdminAppsParams, append: boolean): Promise<void> {
    if (append) {
      loadingMore.set(true);
    } else {
      loading.set(true);
    }
    error.set(null);
    try {
      const page = await api<AdminAppsPage>(`/api/console/admin/apps${adminAppsQuery({ ...params, limit: 50 })}`);
      rows.set(append ? [...rows(), ...page.apps] : page.apps);
      next.set(page.next);
    } catch (err) {
      error.set(err instanceof Error ? err.message : "could not load the apps");
    } finally {
      loading.set(false);
      loadingMore.set(false);
    }
  }

  function search(): void {
    const params = { q: draft().trim(), state: applied().state, plan: applied().plan };
    applied.set(params);
    void load(params, false);
  }

  // First paint. The shell mounts this card only for admins.
  if (rows().length === 0 && next() === null && loading() && error() === null) {
    void load({ q: "", state: "", plan: "" }, false);
  }

  return (
    <section class="card card-border bg-base-100 min-w-0">
      <div class="card-body min-w-0">
        <h2 class="card-title m-0 min-w-0 text-base">All apps</h2>
        <p class="text-sm opacity-70 min-w-0">
          Every app on Outer. Opening one signs you in as its admin: reads and
          writes are audited, and acting on an app needs a fresh sign-in
          (15&nbsp;min) — if the Console sends you through sign-in, come back
          and retry.
        </p>
        <div class="flex min-w-0 flex-wrap items-end gap-2">
          <label class="form-control min-w-0 flex-1">
            <span class="label">
              <span class="label-text">Search slug or owner</span>
            </span>
            <input
              type="search"
              class="input input-bordered min-w-0"
              placeholder="groceries or ana@example.com"
              value={draft()}
              oninput={(e) => draft.set((e.target as HTMLInputElement).value)}
              onkeydown={(e) => {
                if ((e as KeyboardEvent).key === "Enter") search();
              }}
            />
          </label>
          <label class="form-control w-32">
            <span class="label">
              <span class="label-text">State</span>
            </span>
            <select
              class="select select-bordered"
              value={applied().state}
              onchange={(e) => {
                const state = (e.target as HTMLSelectElement).value;
                const params = { q: applied().q, state, plan: applied().plan };
                applied.set(params);
                void load(params, false);
              }}
            >
              <option value="">All states</option>
              <option value="ok">ok</option>
              <option value="held">held</option>
              <option value="suspended">suspended</option>
            </select>
          </label>
          <label class="form-control w-32">
            <span class="label">
              <span class="label-text">Plan</span>
            </span>
            <select
              class="select select-bordered"
              value={applied().plan}
              onchange={(e) => {
                const plan = (e.target as HTMLSelectElement).value;
                const params = { q: applied().q, state: applied().state, plan };
                applied.set(params);
                void load(params, false);
              }}
            >
              <option value="">All plans</option>
              <option value="free">free</option>
              <option value="pro">pro</option>
            </select>
          </label>
          <button class="btn btn-sm btn-neutral" onclick={search}>
            Search
          </button>
          <button
            class="btn btn-sm"
            onclick={() => {
              draft.set("");
              const params = { q: "", state: "", plan: "" };
              applied.set(params);
              void load(params, false);
            }}
          >
            Reset
          </button>
        </div>
        {loading() ? (
          <div class="skeleton h-24 w-full" role="status" aria-label="Loading apps" />
        ) : error() !== null ? (
          <div class="alert alert-error alert-soft min-w-0" role="alert">
            <span class="min-w-0">{error()}</span>
            <button class="btn btn-sm" onclick={search}>
              Retry
            </button>
          </div>
        ) : rows().length === 0 ? (
          <div class="py-6 text-center text-sm opacity-70">No apps match.</div>
        ) : (
          <div class="overflow-x-auto min-w-0">
            <table class="table table-fixed w-full">
              <colgroup>
                <col style="width: 26%" />
                <col style="width: 24%" />
                <col style="width: 10%" />
                <col style="width: 10%" />
                <col style="width: 12%" />
                <col style="width: 18%" />
              </colgroup>
              <thead>
                <tr>
                  <th>App</th>
                  <th>Owner</th>
                  <th>Plan</th>
                  <th>State</th>
                  <th>Last deploy</th>
                  <th>Created</th>
                </tr>
              </thead>
              <tbody>
                {rows().map((row) => (
                  <tr>
                    <td class="min-w-0">
                      <button
                        class="link link-hover truncate font-mono"
                        title={row.slug}
                        onclick={() => onOpenApp(row.slug)}
                      >
                        {row.slug}
                      </button>
                      {row.name !== null && row.name !== "" ? (
                        <div class="truncate text-xs opacity-70" title={row.name}>
                          {row.name}
                        </div>
                      ) : null}
                    </td>
                    <td class="truncate min-w-0" title={row.owner_email}>
                      {row.owner_email}
                    </td>
                    <td>
                      <span class={row.plan === "pro" ? "badge badge-soft badge-info" : "badge badge-soft badge-ghost"}>{row.plan}</span>
                    </td>
                    <td>
                      <span class={row.moderation_state === "suspended" ? "badge badge-soft badge-error" : row.moderation_state === "held" ? "badge badge-soft badge-warning" : "badge badge-soft badge-ghost"}>{row.moderation_state}</span>
                    </td>
                    <td class="truncate min-w-0" title={row.last_deploy?.sha ?? "never deployed"}>
                      {row.last_deploy === null ? (
                        <span class="opacity-60">never</span>
                      ) : (
                        `${row.last_deploy.status} · ${row.last_deploy.sha.slice(0, 8)}`
                      )}
                    </td>
                    <td class="min-w-0">{when(row.created_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {next() !== null && !loading() ? (
          <div class="mt-2">
            <button
              class="btn btn-sm"
              disabled={loadingMore()}
              onclick={() =>
                void load(
                  {
                    q: applied().q,
                    state: applied().state,
                    plan: applied().plan,
                    before: next(),
                  },
                  true,
                )
              }
            >
              {loadingMore() ? "Loading…" : "Load more"}
            </button>
          </div>
        ) : null}
      </div>
    </section>
  );
}
