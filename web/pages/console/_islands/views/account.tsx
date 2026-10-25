// Console account views: connections + API keys, usage + plan, account
// hygiene (spec §13.1, wave 2). Every query/mutation runs through
// `actions/account.ts`; the views only call `run` and render.
import { atom } from "ilha";

import {
  createApiKey,
  deleteAccount,
  revokeApiKey,
  revokeConnection,
  signOutOthers,
} from "../actions/account.ts";
import {
  timeAgo,
  CopyButton,
  type Panel,
} from "../common.tsx";
import {
  when,
  type AccountSummary,
  type ApiKeyEntry,
  type ApiKeysSummary,
  type ConnectionEntry,
  type ConnectionsSummary,
} from "../api.ts";
import { CloseIcon } from "../icons.tsx";

function Loading({ label }: { label: string }) {
  return (
    <div class="flex min-w-0 flex-col gap-3" role="status" aria-label={label}>
      <div class="skeleton rounded-box h-28 w-full" aria-hidden="true" />
      <div class="skeleton rounded-box h-64 w-full" aria-hidden="true" />
      <span class="sr-only">Loading…</span>
    </div>
  );
}

function Failed({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <div class="alert alert-error alert-soft min-w-0" role="alert">
        <span class="min-w-0 break-words">{message}</span>
      </div>
      <div class="min-w-0">
        <button type="button" class="btn btn-sm" onclick={onRetry}>
          Retry
        </button>
      </div>
    </div>
  );
}

function ShellModal({
  label,
  onClose,
  children,
}: {
  label: string;
  onClose: () => void;
  children?: unknown;
}) {
  return (
    <div class="modal modal-open" role="dialog" aria-modal="true" aria-label={label}>
      <div class="modal-box min-w-0">
        <div class="mb-2 flex min-w-0 items-start justify-between gap-2">
          <h3 class="m-0 min-w-0 flex-1 text-base font-semibold">{label}</h3>
          <button
            type="button"
            class="btn btn-ghost btn-square btn-xs shrink-0"
            aria-label="Close"
            onclick={onClose}
          >
            <CloseIcon size={14} />
          </button>
        </div>
        {children}
      </div>
    </div>
  );
}

// --- Connections + API keys --------------------------------------------------

function ConnectionRow({
  entry,
  onRevoke,
}: {
  entry: ConnectionEntry;
  onRevoke: (entry: ConnectionEntry) => void;
}) {
  const label = entry.client_name ?? entry.client_id;
  return (
    <tr>
      <td class="min-w-0">
        <span class="block min-w-0 truncate font-medium" title={label}>
          {label}
        </span>
        {entry.client_name && (
          <span class="block min-w-0 truncate text-xs opacity-60" title={entry.client_id}>
            {entry.client_id}
          </span>
        )}
      </td>
      <td class="min-w-0 truncate max-sm:hidden" title={entry.host ?? "—"}>
        {entry.host ?? "—"}
      </td>
      <td class="min-w-0 truncate max-sm:hidden" title={entry.scope}>
        {entry.scope}
      </td>
      <td class="whitespace-nowrap" title={when(entry.created_at)}>
        {timeAgo(entry.created_at)}
      </td>
      <td class="whitespace-nowrap max-sm:hidden" title={when(entry.last_used_at)}>
        {entry.last_used_at == null ? "never" : timeAgo(entry.last_used_at)}
      </td>
      <td class="text-right">
        <button
          type="button"
          class="btn btn-sm btn-error btn-soft shrink-0"
          onclick={() => onRevoke(entry)}
        >
          Revoke
        </button>
      </td>
    </tr>
  );
}

function ApiKeyRow({
  entry,
  onRevoke,
}: {
  entry: ApiKeyEntry;
  onRevoke: (entry: ApiKeyEntry) => void;
}) {
  return (
    <tr>
      <td class="min-w-0 truncate font-medium" title={entry.name ?? "Unnamed key"}>
        {entry.name ?? "Unnamed key"}
      </td>
      <td class="min-w-0">
        <span class="font-mono text-xs" title={`Prefix ${entry.prefix} — the full key is never shown again`}>
          {entry.prefix}…
        </span>
      </td>
      <td class="whitespace-nowrap max-sm:hidden" title={when(entry.created_at)}>
        {timeAgo(entry.created_at)}
      </td>
      <td class="whitespace-nowrap max-sm:hidden" title={when(entry.last_used_at)}>
        {entry.last_used_at == null ? "never" : timeAgo(entry.last_used_at)}
      </td>
      <td class="text-right">
        <button
          type="button"
          class="btn btn-sm btn-error btn-soft shrink-0"
          onclick={() => onRevoke(entry)}
        >
          Revoke
        </button>
      </td>
    </tr>
  );
}

export function ConnectionsView({
  connections,
  keys,
  onReload,
  onRetry,
}: {
  connections: Panel<ConnectionsSummary>;
  keys: Panel<ApiKeysSummary>;
  onReload: () => void;
  onRetry: () => void;
}) {
  const revokeConn = atom<ConnectionEntry | null>(null);
  const revokeKey = atom<ApiKeyEntry | null>(null);
  const createOpen = atom(false);
  const created = atom<{ prefix: string; api_key: string } | null>(null);
  const busy = atom(false);
  const error = atom<string | null>(null);
  const notice = atom<string | null>(null);

  if (connections.kind === "loading" || keys.kind === "loading") {
    return <Loading label="Loading connections and API keys" />;
  }
  if (connections.kind === "error") {
    return <Failed message={connections.message} onRetry={onRetry} />;
  }
  if (keys.kind === "error") {
    return <Failed message={keys.message} onRetry={onRetry} />;
  }
  const conns = connections.value.connections;
  const list = keys.value.keys;

  async function doRevokeConn(): Promise<void> {
    const target = revokeConn();
    if (!target) {
      return;
    }
    busy.set(true);
    error.set(null);
    try {
      await revokeConnection.run({ id: target.id });
      notice.set(`Revoked access for ${target.client_name ?? target.client_id}.`);
      revokeConn.set(null);
      onReload();
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      busy.set(false);
    }
  }

  async function doRevokeKey(): Promise<void> {
    const target = revokeKey();
    if (!target) {
      return;
    }
    busy.set(true);
    error.set(null);
    try {
      await revokeApiKey.run({ prefix: target.prefix });
      notice.set(`Revoked the key “${target.name ?? target.prefix}”.`);
      revokeKey.set(null);
      onReload();
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      busy.set(false);
    }
  }

  async function doCreate(): Promise<void> {
    const input = document.getElementById("new-key-name") as HTMLInputElement | null;
    busy.set(true);
    error.set(null);
    try {
      const out = await createApiKey.run({ name: (input?.value ?? "").trim() || undefined });
      created.set({ prefix: out.prefix, api_key: out.api_key });
      createOpen.set(false);
      onReload();
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      busy.set(false);
    }
  }

  return (
    <div class="flex min-w-0 flex-col gap-3">
      <div class="alert alert-info alert-soft min-w-0" role="note">
        <span class="min-w-0 break-words">
          This browser is signed in with a web session — revoking a
          connection or key below never signs you out here.
        </span>
      </div>
      {notice() && (
        <div class="alert alert-success alert-soft min-w-0" role="status">
          <span class="min-w-0 break-words">{notice()}</span>
        </div>
      )}
      {error() && (
        <div class="alert alert-error alert-soft min-w-0" role="alert">
          <span class="min-w-0 break-words">{error()}</span>
        </div>
      )}
      <section class="card card-border bg-base-100 min-w-0" aria-label="OAuth connections">
        <div class="card-body min-w-0">
          <h2 class="card-title m-0 text-base">Connections</h2>
          <p class="m-0 min-w-0 text-sm opacity-60">
            Apps and agents signed in with OAuth on your account. Revoking
            signs that client out everywhere.
          </p>
          {conns.length === 0 ? (
            <p class="m-0 min-w-0 py-4 text-center text-sm opacity-60" role="status">
              No connections — sign in with Claude, Codex or ChatGPT to add one.
            </p>
          ) : (
            <div class="min-w-0 overflow-x-auto">
              <table class="table table-fixed w-full max-sm:[&_td]:px-2 max-sm:[&_th]:px-2">
                <colgroup>
                  <col />
                  <col class="max-sm:hidden sm:w-32" />
                  <col class="max-sm:hidden sm:w-20" />
                  <col class="w-20" />
                  <col class="max-sm:hidden sm:w-24" />
                  <col class="w-20" />
                </colgroup>
                <thead>
                  <tr>
                    <th scope="col">Client</th>
                    <th scope="col" class="max-sm:hidden">Host</th>
                    <th scope="col" class="max-sm:hidden">Scope</th>
                    <th scope="col">Added</th>
                    <th scope="col" class="max-sm:hidden">Last used</th>
                    <th scope="col"><span class="sr-only">Actions</span></th>
                  </tr>
                </thead>
                <tbody>
                  {conns.map((entry) => (
                    <ConnectionRow
                      key={entry.id}
                      entry={entry}
                      onRevoke={(target) => {
                        error.set(null);
                        notice.set(null);
                        revokeConn.set(target);
                      }}
                    />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </section>
      <section class="card card-border bg-base-100 min-w-0" aria-label="API keys">
        <div class="card-body min-w-0">
          <div class="flex min-w-0 items-center justify-between gap-2">
            <h2 class="card-title m-0 text-base">API keys</h2>
            <button
              type="button"
              class="btn btn-sm btn-neutral shrink-0"
              onclick={() => {
                error.set(null);
                notice.set(null);
                createOpen.set(true);
              }}
            >
              Create key
            </button>
          </div>
          <p class="m-0 min-w-0 text-sm opacity-60">
            For clients without OAuth: scripts and older agent harnesses.
            Keys are shown once, at creation.
          </p>
          {list.length === 0 ? (
            <p class="m-0 min-w-0 py-4 text-center text-sm opacity-60" role="status">
              No API keys yet.
            </p>
          ) : (
            <div class="min-w-0 overflow-x-auto">
              <table class="table table-fixed w-full max-sm:[&_td]:px-2 max-sm:[&_th]:px-2">
                <colgroup>
                  <col />
                  <col class="w-24" />
                  <col class="max-sm:hidden sm:w-24" />
                  <col class="max-sm:hidden sm:w-24" />
                  <col class="w-20" />
                </colgroup>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col">Prefix</th>
                    <th scope="col" class="max-sm:hidden">Created</th>
                    <th scope="col" class="max-sm:hidden">Last used</th>
                    <th scope="col"><span class="sr-only">Actions</span></th>
                  </tr>
                </thead>
                <tbody>
                  {list.map((entry) => (
                    <ApiKeyRow
                      key={entry.id}
                      entry={entry}
                      onRevoke={(target) => {
                        error.set(null);
                        notice.set(null);
                        revokeKey.set(target);
                      }}
                    />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </section>
      {revokeConn() && (
        <ShellModal label="Revoke connection" onClose={() => revokeConn.set(null)}>
          <p class="m-0 min-w-0 break-words text-sm">
            Revoke access for{" "}
            <strong>{revokeConn()?.client_name ?? revokeConn()?.client_id}</strong>?
            That client signs in again from scratch. This browser stays
            signed in.
          </p>
          <div class="modal-action">
            <button
              type="button"
              class="btn btn-ghost"
              disabled={busy()}
              onclick={() => revokeConn.set(null)}
            >
              Cancel
            </button>
            <button
              type="button"
              class="btn btn-error"
              disabled={busy()}
              onclick={() => void doRevokeConn()}
            >
              {busy() ? "Revoking…" : "Revoke access"}
            </button>
          </div>
        </ShellModal>
      )}
      {revokeKey() && (
        <ShellModal label="Revoke API key" onClose={() => revokeKey.set(null)}>
          <p class="m-0 min-w-0 break-words text-sm">
            Revoke the key “{revokeKey()?.name ?? revokeKey()?.prefix}”
            (<span class="font-mono">{revokeKey()?.prefix}…</span>)? Clients
            using it stop working at once. This browser stays signed in.
          </p>
          <div class="modal-action">
            <button
              type="button"
              class="btn btn-ghost"
              disabled={busy()}
              onclick={() => revokeKey.set(null)}
            >
              Cancel
            </button>
            <button
              type="button"
              class="btn btn-error"
              disabled={busy()}
              onclick={() => void doRevokeKey()}
            >
              {busy() ? "Revoking…" : "Revoke key"}
            </button>
          </div>
        </ShellModal>
      )}
      {createOpen() && (
        <ShellModal label="Create API key" onClose={() => createOpen.set(false)}>
          <label class="form-control min-w-0" for="new-key-name">
            <span class="label min-w-0">
              <span class="label-text truncate" title="A label shown in the key list">
                Name (optional)
              </span>
            </span>
            <input
              type="text"
              id="new-key-name"
              class="input input-bordered w-full min-w-0"
              maxlength="100"
              placeholder="e.g. laptop"
            />
          </label>
          <div class="modal-action">
            <button
              type="button"
              class="btn btn-ghost"
              disabled={busy()}
              onclick={() => createOpen.set(false)}
            >
              Cancel
            </button>
            <button
              type="button"
              class="btn btn-neutral"
              disabled={busy()}
              onclick={() => void doCreate()}
            >
              {busy() ? "Creating…" : "Create key"}
            </button>
          </div>
        </ShellModal>
      )}
      {created() && (
        <ShellModal
          label="Key created — copy it now"
          onClose={() => {
            created.set(null);
            notice.set("Key created.");
          }}
        >
          <p class="m-0 min-w-0 break-words text-sm font-medium text-error">
            You won't see this key again.
          </p>
          <p class="my-2 flex min-w-0 items-center gap-2">
            <code
              class="min-w-0 flex-1 truncate font-mono text-xs"
              title={created()?.api_key}
            >
              {created()?.api_key}
            </code>
            <CopyButton value={created()?.api_key ?? ""} label="Copy API key" />
          </p>
          <div class="modal-action">
            <button
              type="button"
              class="btn btn-neutral"
              onclick={() => {
                created.set(null);
                notice.set("Key created.");
              }}
            >
              Done
            </button>
          </div>
        </ShellModal>
      )}
    </div>
  );
}

// --- Usage + plan ------------------------------------------------------------

function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = units[0];
  for (const next of units) {
    unit = next;
    if (value < 1024) {
      break;
    }
    value /= 1024;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${unit}`;
}

function formatMinutes(ms: number): string {
  const minutes = ms / 60_000;
  if (minutes < 1) {
    return `${Math.round(ms / 1000)}s`;
  }
  if (minutes < 60) {
    return `${minutes >= 10 ? Math.round(minutes) : minutes.toFixed(1)} min`;
  }
  return `${(minutes / 60).toFixed(1)} h`;
}

function QuotaBar({
  label,
  used,
  limit,
  hint,
}: {
  label: string;
  used: number;
  limit: number;
  hint: string;
}) {
  const pct = limit > 0 ? Math.min(100, Math.round((used / limit) * 100)) : 0;
  return (
    <div class="min-w-0">
      <div class="flex min-w-0 items-baseline justify-between gap-2">
        <span class="min-w-0 truncate text-sm font-medium" title={label}>{label}</span>
        <span class="shrink-0 font-mono text-sm" title={hint}>
          {used} / {limit}
        </span>
      </div>
      <progress class="progress w-full" value={used} max={Math.max(limit, 1)} aria-label={`${label}: ${used} of ${limit}`}>
        {pct}%
      </progress>
    </div>
  );
}
export function UsageView({
  panel,
  onRetry,
}: {
  panel: Panel<AccountSummary>;
  onRetry: () => void;
}) {
  const upgradeOpen = atom(false);
  if (panel.kind === "loading") {
    return <Loading label="Loading usage and plan" />;
  }
  if (panel.kind === "error") {
    return <Failed message={panel.message} onRetry={onRetry} />;
  }
  const summary = panel.value;
  const usage = summary.usage;
  const isPro = summary.plan === "pro";
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <section class="card card-border bg-base-100 min-w-0" aria-label="Plan">
        <div class="card-body min-w-0">
          <div class="flex min-w-0 flex-wrap items-center gap-2">
            <h2 class="card-title m-0 min-w-0 flex-1 text-base">Plan</h2>
            <span
              class={`badge badge-soft shrink-0 ${isPro ? "badge-primary" : "badge-neutral"}`}
              title={`On the ${summary.plan} plan`}
            >
              {summary.plan === "pro" ? "Pro" : "Free"}
            </span>
            {summary.plan_status && (
              <span class="badge badge-soft badge-ghost shrink-0" title={`Plan status: ${summary.plan_status}`}>
                {summary.plan_status}
              </span>
            )}
            {!isPro && (
              <button
                type="button"
                class="btn btn-sm btn-neutral shrink-0"
                onclick={() => upgradeOpen.set(true)}
              >
                Upgrade
              </button>
            )}
          </div>
          <p class="m-0 min-w-0 truncate text-sm opacity-60" title={`Member since ${when(summary.member_since)}`}>
            Member since {when(summary.member_since)}
          </p>
        </div>
      </section>
      <section class="card card-border bg-base-100 min-w-0" aria-label="Today's usage">
        <div class="card-body min-w-0 gap-4">
          <h2 class="card-title m-0 text-base">Today</h2>
          <QuotaBar
            label="Code changes"
            used={usage.code_changes_today}
            limit={usage.code_changes_limit}
            hint={`Resets at ${summary.resets_at}`}
          />
          <QuotaBar
            label="Apps owned"
            used={usage.apps_owned}
            limit={usage.apps_limit}
            hint="App slots on your plan"
          />
          {usage.rollbacks.length === 0 ? (
            <p class="m-0 min-w-0 text-sm opacity-60">No apps yet — nothing to roll back.</p>
          ) : (
            usage.rollbacks.map((row) => (
              <QuotaBar
                key={row.slug}
                label={`Rollbacks · ${row.slug}`}
                used={row.used_today}
                limit={row.limit}
                hint="20 rollbacks per app per day"
              />
            ))
          )}
          <div class="min-w-0">
            <div class="flex min-w-0 items-baseline justify-between gap-2">
              <span class="min-w-0 truncate text-sm font-medium" title="Build minutes charged today">
                Build minutes today
              </span>
              <span class="shrink-0 font-mono text-sm">{formatMinutes(usage.build_ms_today)}</span>
            </div>
            <p class="m-0 min-w-0 text-xs opacity-60">Tracked, not yet capped.</p>
          </div>
        </div>
      </section>
      <section class="card card-border bg-base-100 min-w-0" aria-label="This month">
        <div class="card-body min-w-0">
          <h2 class="card-title m-0 text-base">This month</h2>
          <dl class="m-0 grid min-w-0 grid-cols-1 gap-2 sm:grid-cols-3">
            <div class="min-w-0">
              <dt class="truncate text-xs opacity-60" title="App requests served this month">Requests</dt>
              <dd class="m-0 truncate font-mono text-lg" title={String(usage.requests_month)}>
                {usage.requests_month.toLocaleString()}
              </dd>
            </div>
            <div class="min-w-0">
              <dt class="truncate text-xs opacity-60" title="Largest app storage footprint seen this month">Storage peak</dt>
              <dd class="m-0 truncate font-mono text-lg" title={`${usage.storage_peak_bytes} bytes`}>
                {formatBytes(usage.storage_peak_bytes)}
              </dd>
            </div>
            <div class="min-w-0">
              <dt class="truncate text-xs opacity-60" title="Outbound traffic from your apps this month">Egress</dt>
              <dd class="m-0 truncate font-mono text-lg" title={`${usage.egress_bytes_month} bytes`}>
                {formatBytes(usage.egress_bytes_month)}
              </dd>
            </div>
          </dl>
          <p class="m-0 min-w-0 text-xs opacity-60">
            Metering is still being wired up — these counts are tracked, not billed.
          </p>
        </div>
      </section>
      {upgradeOpen() && (
        <ShellModal label="Outer Pro" onClose={() => upgradeOpen.set(false)}>
          <ul class="m-0 min-w-0 list-disc space-y-1 pl-5 text-sm">
            <li>10 app slots, 500 code changes a day</li>
            <li>Public visibility, collaborators and custom domains</li>
            <li>Code export, R2 file uploads, longer log retention</li>
          </ul>
          <p class="m-0 min-w-0 break-words text-sm opacity-60">
            Pro is $8/month. Billing isn't wired up yet — there is nothing
            to pay today, and this button goes nowhere on purpose.
          </p>
          <div class="modal-action">
            <button
              type="button"
              class="btn btn-neutral"
              onclick={() => upgradeOpen.set(false)}
            >
              Got it
            </button>
          </div>
        </ShellModal>
      )}
    </div>
  );
}

// --- Account -----------------------------------------------------------------

export function AccountView({
  panel,
  onRetry,
}: {
  panel: Panel<AccountSummary>;
  onRetry: () => void;
}) {
  const signOutOpen = atom(false);
  const deleteOpen = atom(false);
  const typed = atom("");
  const busy = atom(false);
  const error = atom<string | null>(null);
  const notice = atom<string | null>(null);

  if (panel.kind === "loading") {
    return <Loading label="Loading account" />;
  }
  if (panel.kind === "error") {
    return <Failed message={panel.message} onRetry={onRetry} />;
  }
  const summary = panel.value;

  async function doSignOutOthers(): Promise<void> {
    busy.set(true);
    error.set(null);
    try {
      const out = await signOutOthers.run();
      notice.set(
        out.signed_out === 0
          ? "No other sessions — only this browser is signed in."
          : `Signed out ${out.signed_out} other session${out.signed_out === 1 ? "" : "s"}.`,
      );
      signOutOpen.set(false);
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      busy.set(false);
    }
  }

  async function doDelete(): Promise<void> {
    busy.set(true);
    error.set(null);
    try {
      await deleteAccount.run({ confirm: typed().trim() });
      globalThis.location.assign("/console/sign-in");
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      busy.set(false);
    }
  }

  const typedMatches =
    typed().trim().toLowerCase() === summary.email.toLowerCase();

  return (
    <div class="flex min-w-0 flex-col gap-3">
      {notice() && (
        <div class="alert alert-success alert-soft min-w-0" role="status">
          <span class="min-w-0 break-words">{notice()}</span>
        </div>
      )}
      {error() && (
        <div class="alert alert-error alert-soft min-w-0" role="alert">
          <span class="min-w-0 break-words">{error()}</span>
        </div>
      )}
      <section class="card card-border bg-base-100 min-w-0" aria-label="Account">
        <div class="card-body min-w-0">
          <h2 class="card-title m-0 text-base">Account</h2>
          <dl class="m-0 grid min-w-0 grid-cols-1 gap-2 sm:grid-cols-2">
            <div class="min-w-0">
              <dt class="truncate text-xs opacity-60">Email</dt>
              <dd class="m-0 min-w-0 truncate" title={summary.email}>{summary.email}</dd>
            </div>
            <div class="min-w-0">
              <dt class="truncate text-xs opacity-60">Member since</dt>
              <dd class="m-0 min-w-0 truncate" title={when(summary.member_since)}>
                {when(summary.member_since)}
              </dd>
            </div>
          </dl>
          <div class="min-w-0">
            <button
              type="button"
              class="btn btn-sm shrink-0"
              onclick={() => {
                error.set(null);
                notice.set(null);
                signOutOpen.set(true);
              }}
            >
              Sign out of all other sessions
            </button>
          </div>
        </div>
      </section>
      <section
        class="card card-border border-error bg-base-100 min-w-0"
        aria-label="Danger zone"
      >
        <div class="card-body min-w-0">
          <h2 class="card-title m-0 text-base text-error">Danger zone</h2>
          <p class="m-0 min-w-0 text-sm opacity-70">
            Deleting your account removes your apps and signs every session
            out. Apps shared with you stay with their owners. This cannot be
            undone.
          </p>
          <div class="min-w-0">
            <button
              type="button"
              class="btn btn-sm btn-error shrink-0"
              onclick={() => {
                error.set(null);
                notice.set(null);
                typed.set("");
                deleteOpen.set(true);
              }}
            >
              Delete account
            </button>
          </div>
        </div>
      </section>
      {signOutOpen() && (
        <ShellModal label="Sign out other sessions" onClose={() => signOutOpen.set(false)}>
          <p class="m-0 min-w-0 break-words text-sm">
            Sign out every other browser and device on this account? This
            browser stays signed in.
          </p>
          <div class="modal-action">
            <button
              type="button"
              class="btn btn-ghost"
              disabled={busy()}
              onclick={() => signOutOpen.set(false)}
            >
              Cancel
            </button>
            <button
              type="button"
              class="btn btn-neutral"
              disabled={busy()}
              onclick={() => void doSignOutOthers()}
            >
              {busy() ? "Signing out…" : "Sign out others"}
            </button>
          </div>
        </ShellModal>
      )}
      {deleteOpen() && (
        <ShellModal label="Delete account" onClose={() => deleteOpen.set(false)}>
          <p class="m-0 min-w-0 break-words text-sm">
            This deletes your apps, API keys and connections, and signs
            every session out. Type your email to confirm.
          </p>
          <label class="form-control min-w-0" for="delete-confirm-email">
            <span class="label min-w-0">
              <span class="label-text truncate" title="Type your email exactly">Email</span>
            </span>
            <input
              type="email"
              id="delete-confirm-email"
              class="input input-bordered w-full min-w-0"
              maxlength="320"
              placeholder={summary.email}
              value={typed()}
              oninput={(e: Event) => typed.set((e.target as HTMLInputElement).value)}
            />
          </label>
          <div class="modal-action">
            <button
              type="button"
              class="btn btn-ghost"
              disabled={busy()}
              onclick={() => deleteOpen.set(false)}
            >
              Cancel
            </button>
            <button
              type="button"
              class="btn btn-error"
              disabled={busy() || !typedMatches}
              onclick={() => void doDelete()}
            >
              {busy() ? "Deleting…" : "Delete my account"}
            </button>
          </div>
        </ShellModal>
      )}
    </div>
  );
}
