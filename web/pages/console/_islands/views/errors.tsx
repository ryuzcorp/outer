// Console Errors view: Open / Resolved / Ignored tabs with counts,
// per-issue and bulk resolve/ignore/reopen, and an expandable detail
// (message, top frame, first/last seen, count, an hourly sparkline, and
// the most recent matching log lines).
import { atom } from "ilha";

import {
  action,
  consumeJumpToIssue,
  type BulkTriageInput,
  type BulkTriageResult,
  type ErrorDetailInput,
  type IssueDetail,
  type ListErrorsInput,
  type TriageInput,
  type TriageResult,
} from "../actions/observe.ts";
import { timeAgo, type Panel } from "../common.tsx";
import { when, type ErrorCounts, type ErrorIssue, type ErrorsSummary, type SparkBucket } from "../api.ts";
import { ErrorsIcon } from "../icons.tsx";
import { EmptyState, Err, Loading } from "../views.tsx";

type Tab = "open" | "resolved" | "ignored";
const TABS: Tab[] = ["open", "resolved", "ignored"];

function issueBadge(status: string): string {
  if (status === "open") {
    return "badge-error";
  }
  if (status === "resolved") {
    return "badge-success";
  }
  return "badge-ghost";
}

function tabLabel(tab: Tab, counts: ErrorCounts): string {
  const n = tab === "open" ? counts.open : tab === "resolved" ? counts.resolved : counts.ignored;
  return `${tab} (${n.toLocaleString()})`;
}

function Sparkline({ buckets }: { buckets: SparkBucket[] }) {
  const max = Math.max(1, ...buckets.map((b) => b.count));
  return (
    <svg viewBox="0 0 120 28" class="h-7 w-30 opacity-70" role="img" aria-label="Occurrences per hour, trailing day">
      {buckets.map((b, i) => {
        const height = Math.round((b.count / max) * 26);
        return (
          <rect
            key={b.start}
            x={i * 5 + 1}
            y={27 - height}
            width={3}
            height={Math.max(height, b.count > 0 ? 2 : 0)}
            fill="currentColor"
          >
            <title>{`${b.count} at ${new Date(b.start).toLocaleString()}`}</title>
          </rect>
        );
      })}
    </svg>
  );
}

export function ErrorsView({
  slug,
  panel,
  onRetry,
}: {
  slug: string;
  panel: Panel<ErrorsSummary>;
  onRetry: () => void;
}) {
  const ready = panel.kind === "ready" ? panel.value : null;
  const tab = atom<Tab>("open");
  const issues = atom<ErrorIssue[]>(ready ? ready.issues.filter((i) => i.status === "open") : []);
  const counts = atom<ErrorCounts>(ready ? ready.counts : { open: 0, resolved: 0, ignored: 0 });
  const s5xx = atom(ready ? ready.s5xx_last_hour : 0);
  const adopted = atom(ready !== null);
  const busy = atom(panel.kind === "loading");
  const error = atom<string | null>(panel.kind === "error" ? panel.message : null);
  const selected = atom<string[]>([]);
  const expanded = atom<string | null>(null);
  const detail = atom<IssueDetail | null>(null);
  const detailBusy = atom(false);
  const mutating = atom<string | null>(null);
  const jumpBusy = atom(false);

  async function loadTab(next: Tab): Promise<void> {
    tab.set(next);
    busy.set(true);
    error.set(null);
    selected.set([]);
    try {
      const summary = await action<ListErrorsInput, ErrorsSummary>("list_errors").run({ slug, status: next });
      issues.set(summary.issues);
      counts.set(summary.counts);
      s5xx.set(summary.s5xx_last_hour);
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      busy.set(false);
    }
  }

  // Adopt the shell's first load once; tab switches and mutations fetch.
  if (!adopted() && panel.kind === "ready") {
    adopted.set(true);
    busy.set(false);
    counts.set(panel.value.counts);
    s5xx.set(panel.value.s5xx_last_hour);
    if (tab() === "open") {
      issues.set(panel.value.issues.filter((i) => i.status === "open"));
    }
  }

  // A "View issue" jump from Logs: fetch the issue wherever it lives,
  // switch to its tab, and expand it.
  const jump = consumeJumpToIssue();
  if (jump !== null && !jumpBusy()) {
    jumpBusy.set(true);
    void (async () => {
      try {
        const found = await action<ErrorDetailInput, IssueDetail>("error_detail").run({
          slug,
          fingerprint: jump,
        });
        await loadTab(found.issue.status as Tab);
        detail.set(found);
        expanded.set(found.issue.fingerprint);
      } catch (err: unknown) {
        error.set(err instanceof Error ? err.message : String(err));
      } finally {
        jumpBusy.set(false);
      }
    })();
  }

  async function toggleExpand(issue: ErrorIssue): Promise<void> {
    if (expanded() === issue.fingerprint) {
      expanded.set(null);
      detail.set(null);
      return;
    }
    expanded.set(issue.fingerprint);
    detail.set(null);
    detailBusy.set(true);
    try {
      detail.set(
        await action<ErrorDetailInput, IssueDetail>("error_detail").run({ slug, fingerprint: issue.fingerprint }),
      );
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
      expanded.set(null);
    } finally {
      detailBusy.set(false);
    }
  }

  async function triageOne(issue: ErrorIssue, name: "resolve_error" | "ignore_error" | "reopen_error"): Promise<void> {
    mutating.set(issue.fingerprint);
    error.set(null);
    try {
      await action<TriageInput, TriageResult>(name).run({ slug, fingerprint: issue.fingerprint });
      await loadTab(tab());
      if (expanded() === issue.fingerprint) {
        expanded.set(null);
        detail.set(null);
      }
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      mutating.set(null);
    }
  }

  async function triageBulk(name: "resolve" | "ignore" | "reopen"): Promise<void> {
    const fps = selected();
    if (fps.length === 0) {
      return;
    }
    mutating.set("bulk");
    error.set(null);
    try {
      await action<BulkTriageInput, BulkTriageResult>("bulk_triage").run({ slug, action: name, fingerprints: fps });
      await loadTab(tab());
    } catch (err: unknown) {
      error.set(err instanceof Error ? err.message : String(err));
    } finally {
      mutating.set(null);
    }
  }

  function toggleSelect(fp: string): void {
    const current = selected();
    selected.set(current.includes(fp) ? current.filter((x) => x !== fp) : [...current, fp]);
  }

  if (busy() && issues().length === 0 && !error()) {
    return <Loading label="Loading errors" />;
  }
  if (error() && issues().length === 0) {
    return <Err message={error() as string} onRetry={onRetry} />;
  }
  const shown = issues();
  const checked = selected();
  const allChecked = shown.length > 0 && shown.every((i) => checked.includes(i.fingerprint));
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <section class="card card-border bg-base-100 min-w-0" aria-label="Errors">
        <div class="card-body min-w-0">
          <div class="mb-3 flex min-w-0 flex-wrap items-center gap-2">
            <div class="join min-w-0 shrink-0" role="tablist" aria-label="Issue status">
              {TABS.map((t) => (
                <button
                  key={t}
                  type="button"
                  role="tab"
                  aria-selected={tab() === t}
                  class={`btn btn-sm join-item ${tab() === t ? "btn-neutral" : "btn-ghost"}`}
                  disabled={busy()}
                  onclick={() => void loadTab(t)}
                >
                  {tabLabel(t, counts())}
                </button>
              ))}
            </div>
            <span
              class="min-w-0 flex-1 truncate text-right text-xs opacity-60"
              title={`${s5xx().toLocaleString()} proxy 5xx in the last hour`}
            >
              {s5xx().toLocaleString()} proxy 5xx in the last hour
            </span>
          </div>
          {error() && (
            <p class="m-0 mb-3 min-w-0 break-words text-sm text-error" role="alert">
              {error()}
            </p>
          )}
          {checked.length > 0 && (
            <div class="mb-3 flex min-w-0 flex-wrap items-center gap-2 rounded-field bg-base-200 p-2" role="toolbar" aria-label="Bulk triage">
              <span class="min-w-0 truncate text-sm" title={`${checked.length} selected`}>
                {checked.length} selected
              </span>
              {(["resolve", "ignore", "reopen"] as const).map((name) => (
                <button
                  key={name}
                  type="button"
                  class="btn btn-sm shrink-0"
                  disabled={mutating() !== null}
                  onclick={() => void triageBulk(name)}
                >
                  {mutating() === "bulk" ? "Working…" : name === "resolve" ? "Resolve" : name === "ignore" ? "Ignore" : "Reopen"}
                </button>
              ))}
              <button type="button" class="btn btn-sm btn-ghost shrink-0" onclick={() => selected.set([])}>
                Clear
              </button>
            </div>
          )}
          {shown.length === 0 ? (
            <EmptyState icon={<ErrorsIcon />} title={`No ${tab()} errors`}>
              {tab() === "open"
                ? `Grouped runtime errors will show up here.${s5xx() > 0 ? ` ${s5xx()} proxy 5xx in the last hour, with no grouped issue.` : ""}`
                : `Nothing is ${tab()} right now.`}
            </EmptyState>
          ) : (
            <div class="min-w-0 overflow-x-auto">
              <table class="table table-fixed w-full">
                <colgroup>
                  <col class="w-10" />
                  <col class="w-24" />
                  <col />
                  <col class="w-20" />
                  <col class="w-24" />
                  <col class="w-28" />
                </colgroup>
                <thead>
                  <tr>
                    <th scope="col">
                      <input
                        type="checkbox"
                        class="checkbox checkbox-sm"
                        aria-label="Select all issues"
                        checked={allChecked}
                        onchange={() => {
                          selected.set(allChecked ? [] : shown.map((i) => i.fingerprint));
                        }}
                      />
                    </th>
                    <th scope="col">Last seen</th>
                    <th scope="col">Error</th>
                    <th scope="col">Seen</th>
                    <th scope="col">Status</th>
                    <th scope="col">
                      <span class="sr-only">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {shown.map((issue) => {
                    const isOpen = expanded() === issue.fingerprint;
                    return (
                      <>
                        <tr key={issue.fingerprint}>
                          <td>
                            <input
                              type="checkbox"
                              class="checkbox checkbox-sm"
                              aria-label={`Select ${issue.message}`}
                              checked={checked.includes(issue.fingerprint)}
                              onchange={() => toggleSelect(issue.fingerprint)}
                            />
                          </td>
                          <td class="whitespace-nowrap" title={when(issue.last_seen)}>
                            {timeAgo(issue.last_seen)}
                          </td>
                          <td class="min-w-0">
                            <button
                              type="button"
                              class="block w-full min-w-0 text-left font-mono"
                              title={issue.frame ? `${issue.message}\n${issue.frame}` : issue.message}
                              onclick={() => void toggleExpand(issue)}
                              aria-expanded={isOpen}
                            >
                              <span class="block truncate">{issue.message}</span>
                              {issue.frame && (
                                <span class="block truncate opacity-60" title={issue.frame}>
                                  {issue.frame}
                                </span>
                              )}
                            </button>
                          </td>
                          <td class="whitespace-nowrap">{issue.count.toLocaleString()}×</td>
                          <td class="min-w-0">
                            <span class="flex min-w-0 flex-wrap items-center gap-1">
                              <span class={`badge badge-soft ${issueBadge(issue.status)} shrink-0`} title={issue.status}>
                                {issue.status}
                              </span>
                              {issue.regressed && (
                                <span class="badge badge-soft badge-warning shrink-0" title="Resolved before, then recurred">
                                  Regressed
                                </span>
                              )}
                            </span>
                          </td>
                          <td class="whitespace-nowrap">
                            {mutating() === issue.fingerprint ? (
                              <span class="text-xs opacity-60">Working…</span>
                            ) : (
                              <span class="flex items-center justify-end gap-1">
                                {issue.status === "open" ? (
                                  <>
                                    <button type="button" class="btn btn-ghost btn-xs" title="Resolve" onclick={() => void triageOne(issue, "resolve_error")}>
                                      Resolve
                                    </button>
                                    <button type="button" class="btn btn-ghost btn-xs" title="Ignore" onclick={() => void triageOne(issue, "ignore_error")}>
                                      Ignore
                                    </button>
                                  </>
                                ) : (
                                  <>
                                    <button type="button" class="btn btn-ghost btn-xs" title="Reopen" onclick={() => void triageOne(issue, "reopen_error")}>
                                      Reopen
                                    </button>
                                    {issue.status === "resolved" ? (
                                      <button type="button" class="btn btn-ghost btn-xs" title="Ignore" onclick={() => void triageOne(issue, "ignore_error")}>
                                        Ignore
                                      </button>
                                    ) : (
                                      <button type="button" class="btn btn-ghost btn-xs" title="Resolve" onclick={() => void triageOne(issue, "resolve_error")}>
                                        Resolve
                                      </button>
                                    )}
                                  </>
                                )}
                              </span>
                            )}
                          </td>
                        </tr>
                        {isOpen && (
                          <tr key={`${issue.fingerprint}-detail`}>
                            <td colspan="6" class="min-w-0 bg-base-200">
                              {detailBusy() || !detail() || detail()?.issue.fingerprint !== issue.fingerprint ? (
                                <span class="loading loading-spinner loading-sm" aria-label="Loading issue detail" />
                              ) : (
                                <div class="flex min-w-0 flex-col gap-2 py-1">
                                  <p class="m-0 min-w-0 break-words font-mono text-sm" title={detail()?.issue.message}>
                                    {detail()?.issue.message}
                                  </p>
                                  {detail()?.issue.frame && (
                                    <p class="m-0 min-w-0 truncate font-mono text-xs opacity-60" title={detail()?.issue.frame ?? ""}>
                                      {detail()?.issue.frame}
                                    </p>
                                  )}
                                  <p class="m-0 min-w-0 text-xs opacity-60">
                                    First seen <span title={when(detail()?.issue.first_seen ?? 0)}>{timeAgo(detail()?.issue.first_seen ?? 0)}</span>
                                    {" · "}last seen <span title={when(detail()?.issue.last_seen ?? 0)}>{timeAgo(detail()?.issue.last_seen ?? 0)}</span>
                                    {" · "}{detail()?.issue.count.toLocaleString()}×
                                  </p>
                                  <div class="flex min-w-0 items-center gap-2">
                                    <Sparkline buckets={detail()?.sparkline ?? []} />
                                    <span class="min-w-0 truncate text-xs opacity-60">occurrences per hour, trailing day</span>
                                  </div>
                                  {(detail()?.recent.length ?? 0) > 0 && (
                                    <ol class="log-list">
                                      {(detail()?.recent ?? []).map((line) => (
                                        <li key={line.seq} class={`log-line is-${line.stream}`}>
                                          <span class="log-stream" aria-hidden="true">
                                            {line.stream === "stderr" ? "!" : ">"}
                                          </span>
                                          <code>{line.line}</code>
                                        </li>
                                      ))}
                                    </ol>
                                  )}
                                </div>
                              )}
                            </td>
                          </tr>
                        )}
                      </>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </section>
    </div>
  );
}
