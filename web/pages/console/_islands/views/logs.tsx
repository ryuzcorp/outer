// Console Logs view: server-side search, stream/level/time filters,
// backwards pagination, wrap toggle, per-line copy, jump-to-issue, and a
// Follow live tail over SSE.
//
// Follow stops when the tab hides (the EventSource closes, so the server
// ends the stream) and reconnects with backoff on drops; the stream is
// same-origin, inside the console CSP's `connect-src 'self'`.
import { atom, watch } from "ilha";

import {
  action,
  followLogs,
  type LogsPage,
  type QueryLogsInput,
} from "../actions/observe.ts";
import { CopyButton, type Panel } from "../common.tsx";
import { when, type LogLine } from "../api.ts";
import { LogsIcon } from "../icons.tsx";
import { EmptyState, Err, Loading } from "../views.tsx";

const PAGE_SIZE = 100;
const MAX_KEPT = 1000;

export function LogsView({
  slug,
  panel,
  onRetry,
  onJumpToIssue,
}: {
  slug: string;
  panel: Panel<LogLine[]>;
  onRetry: () => void;
  onJumpToIssue: (fingerprint: string) => void;
}) {
  const lines = atom<LogLine[] | null>(panel.kind === "ready" ? panel.value : null);
  const synced = atom<LogLine[] | null>(panel.kind === "ready" ? panel.value : null);
  const dirty = atom(false);
  const busy = atom(panel.kind === "loading");
  const error = atom<string | null>(panel.kind === "error" ? panel.message : null);
  const hasMore = atom(panel.kind === "ready" && panel.value.length >= PAGE_SIZE);
  const following = atom(false);
  const followNote = atom<string | null>(null);
  const wrap = atom(true);
  const q = atom("");
  const stream = atom("");
  const level = atom("");
  const range = atom("all");

  // Adopt the shell's quiet poll until the user takes over (filter,
  // pagination or follow): the view owns fetching after that.
  if (!dirty() && panel.kind === "ready" && synced() !== panel.value) {
    synced.set(panel.value);
    lines.set(panel.value);
    busy.set(false);
    hasMore.set(panel.value.length >= PAGE_SIZE);
  }
  if (!dirty() && panel.kind === "loading" && lines() === null) {
    busy.set(true);
  }

  let ctrl: AbortController | null = null;
  function stopFollow(note: string | null = null): void {
    ctrl?.abort();
    ctrl = null;
    if (following()) {
      following.set(false);
    }
    followNote.set(note);
  }
  function startFollow(): void {
    stopFollow();
    const tail = lines() ?? [];
    const since = tail.length > 0 ? tail[tail.length - 1].seq : null;
    const next = new AbortController();
    ctrl = next;
    following.set(true);
    followNote.set(null);
    followLogs(
      slug,
      { q: q() || undefined, stream: stream() || undefined, level: level() || undefined, ...rangeFrom(range()) },
      since,
      (fresh: LogLine[]) => {
        const merged = [...(lines() ?? []), ...fresh].slice(-MAX_KEPT);
        lines.set(merged);
        followNote.set(null);
        const list = document.getElementById("log-list");
        if (list) {
          list.scrollTop = list.scrollHeight;
        }
      },
      (message: string) => {
        followNote.set(message);
      },
      next.signal,
    );
  }

  // Follow ends with the tab: hidden stops the stream (which ends it
  // server-side), visible restarts it when follow is still on. Unmount
  // stops it for good.
  watch.once(() => {
    const onVisibility = () => {
      if (document.hidden) {
        ctrl?.abort();
        ctrl = null;
      } else if (following() && !ctrl) {
        startFollow();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      stopFollow();
    };
  });

  function rangeFrom(value: string): { from?: number } {
    if (value === "1h") {
      return { from: Date.now() - 3_600_000 };
    }
    if (value === "24h") {
      return { from: Date.now() - 86_400_000 };
    }
    return {};
  }

  async function load(input: QueryLogsInput, append: "replace" | "older"): Promise<void> {
    busy.set(true);
    error.set(null);
    try {
      const page = await action<QueryLogsInput, LogsPage>("query_logs").run(input);
      if (append === "replace") {
        lines.set(page.lines);
      } else {
        lines.set([...page.lines, ...(lines() ?? [])]);
      }
      hasMore.set(page.lines.length >= (input.limit ?? PAGE_SIZE));
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      busy.set(false);
    }
  }

  function currentFilter(): QueryLogsInput {
    return {
      slug,
      q: q() || undefined,
      stream: stream() || undefined,
      level: level() || undefined,
      ...rangeFrom(range()),
      limit: PAGE_SIZE,
    };
  }

  function applyFilters(): void {
    dirty.set(true);
    stopFollow();
    void load(currentFilter(), "replace");
  }

  function loadOlder(): void {
    const shown = lines() ?? [];
    if (shown.length === 0) {
      return;
    }
    const oldest = shown[0].seq;
    dirty.set(true);
    stopFollow();
    void load({ ...currentFilter(), before: oldest }, "older");
  }

  function refresh(): void {
    dirty.set(true);
    if (following()) {
      startFollow();
      return;
    }
    void load(currentFilter(), "replace");
  }

  if (busy() && lines() === null) {
    return <Loading label="Loading logs" />;
  }
  if (error() && lines() === null) {
    return <Err message={error() as string} onRetry={onRetry} />;
  }
  const shown = lines() ?? [];
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <section class="card card-border bg-base-100 min-w-0" aria-label="Log filters">
        <div class="card-body min-w-0 gap-3">
          <div class="flex min-w-0 flex-col gap-2 lg:flex-row">
            <label class="form-control min-w-0 flex-1" for="log-search">
              <span class="label min-w-0">
                <span class="label-text truncate" title="Substring match, case-insensitive">
                  Search
                </span>
              </span>
              <input
                type="search"
                id="log-search"
                class="input input-bordered w-full min-w-0"
                placeholder="e.g. disk full"
                maxlength="200"
                value={q()}
                oninput={(e: Event) => q.set((e.target as HTMLInputElement).value)}
                onkeydown={(e: KeyboardEvent) => {
                  if (e.key === "Enter") {
                    applyFilters();
                  }
                }}
              />
            </label>
            <div class="flex min-w-0 flex-1 flex-col gap-2 sm:flex-row sm:items-end">
              <label class="form-control min-w-0 flex-1" for="log-stream">
                <span class="label min-w-0">
                  <span class="label-text truncate">Stream</span>
                </span>
                <select
                  id="log-stream"
                  class="select select-bordered w-full min-w-0"
                  value={stream()}
                  onchange={(e: Event) => {
                    stream.set((e.target as HTMLSelectElement).value);
                    applyFilters();
                  }}
                >
                  <option value="">stdout + stderr</option>
                  <option value="stdout">stdout</option>
                  <option value="stderr">stderr</option>
                </select>
              </label>
              <label class="form-control min-w-0 flex-1" for="log-level">
                <span class="label min-w-0">
                  <span class="label-text truncate">Level</span>
                </span>
                <select
                  id="log-level"
                  class="select select-bordered w-full min-w-0"
                  value={level()}
                  onchange={(e: Event) => {
                    level.set((e.target as HTMLSelectElement).value);
                    applyFilters();
                  }}
                >
                  <option value="">all levels</option>
                  <option value="error">error</option>
                  <option value="warn">warn</option>
                  <option value="info">info</option>
                </select>
              </label>
              <label class="form-control min-w-0 flex-1" for="log-range">
                <span class="label min-w-0">
                  <span class="label-text truncate">Time</span>
                </span>
                <select
                  id="log-range"
                  class="select select-bordered w-full min-w-0"
                  value={range()}
                  onchange={(e: Event) => {
                    range.set((e.target as HTMLSelectElement).value);
                    applyFilters();
                  }}
                >
                  <option value="all">all retained</option>
                  <option value="1h">last hour</option>
                  <option value="24h">last 24 hours</option>
                </select>
              </label>
            </div>
          </div>
          <div class="flex min-w-0 flex-wrap items-center gap-2">
            <button type="button" class="btn btn-sm btn-neutral shrink-0" disabled={busy()} onclick={applyFilters}>
              Search
            </button>
            <button type="button" class="btn btn-sm shrink-0" disabled={busy()} onclick={refresh}>
              Refresh
            </button>
            <button
              type="button"
              class={`btn btn-sm shrink-0 ${following() ? "btn-error" : "btn-primary"}`}
              onclick={() => {
                dirty.set(true);
                if (following()) {
                  stopFollow();
                } else {
                  startFollow();
                }
              }}
            >
              {following() ? "Stop follow" : "Follow"}
            </button>
            <label class="label shrink-0 cursor-pointer gap-2" for="log-wrap">
              <input
                type="checkbox"
                id="log-wrap"
                class="checkbox checkbox-sm"
                checked={wrap()}
                onchange={(e: Event) => wrap.set((e.target as HTMLInputElement).checked)}
              />
              <span class="label-text">Wrap</span>
            </label>
            <span class="min-w-0 flex-1 truncate text-xs opacity-60" title={`${shown.length.toLocaleString()} lines shown`}>
              {shown.length.toLocaleString()} lines shown
              {following() ? " · live" : ""}
            </span>
          </div>
          {error() && (
            <p class="m-0 min-w-0 break-words text-sm text-error" role="alert">
              {error()}
            </p>
          )}
          {followNote() && (
            <p class="m-0 min-w-0 break-words text-xs opacity-60" role="status">
              {followNote()}
            </p>
          )}
        </div>
      </section>
      <section class="card card-border bg-base-100 min-w-0" aria-label="Logs">
        <div class="card-body min-w-0">
          {shown.length === 0 ? (
            <EmptyState plain icon={<LogsIcon />} title="No log lines match">
              Lines your app writes will show up here.
            </EmptyState>
          ) : (
            <ol id="log-list" class={`log-list${wrap() ? "" : " nowrap"}`}>
              {shown.map((entry) => (
                <li
                  key={entry.seq}
                  class={`log-line is-${entry.stream}`}
                  title={when(entry.ts)}
                >
                  <span class="log-stream" aria-hidden="true">
                    {entry.stream === "stderr" ? "!" : ">"}
                  </span>
                  <code class="min-w-0 flex-1">{entry.line}</code>
                  <span class="flex shrink-0 items-start gap-1">
                    {entry.fingerprint && (
                      <button
                        type="button"
                        class="btn btn-ghost btn-xs shrink-0"
                        title={`View issue ${entry.fingerprint}`}
                        onclick={() => onJumpToIssue(entry.fingerprint as string)}
                      >
                        Issue
                      </button>
                    )}
                    <CopyButton value={entry.line} label="Copy line" />
                  </span>
                </li>
              ))}
            </ol>
          )}
          {hasMore() && shown.length > 0 && (
            <div class="mt-3 flex min-w-0 justify-center">
              <button type="button" class="btn btn-sm shrink-0" disabled={busy()} onclick={loadOlder}>
                {busy() ? "Loading…" : "Load older"}
              </button>
            </div>
          )}
        </div>
      </section>
    </div>
  );
}
