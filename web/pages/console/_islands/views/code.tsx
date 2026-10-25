// Console Code view (wave 2 (e)): a read-only window into the app's repo.
//
// Tabs: Files (version picker + tree + viewer with line numbers) and Changes
// (one commit's files with +/- counts plus its unified patch, with an
// optional compare-against-live). Everything loads through the registered
// actions in `../actions/code.ts`, never by direct fetch.
//
// No syntax highlighting library: the console CSP only hashes inline
// scripts/styles, so a highlighter would churn the bundle and the CSP for
// cosmetic value. Plain monospace with line numbers keeps every byte
// server-rendered and the CSP intact.
import { atom, watch } from "ilha";
import {
  shortSha,
  when,
  type ConsoleDeploy,
} from "../api.ts";
import { CopyButton, timeAgo, type Panel } from "../common.tsx";
import {
  actions,
  type CompareResult,
  type DiffFile,
  type DiffResult,
  type FileResult,
  type HistoryResult,
  type TreeResult,
} from "../actions/code.ts";
import { DeploysIcon } from "../icons.tsx";

const [treeAction, fileAction, historyAction, diffAction, compareAction] = actions;

function Loading({ label }: { label: string }) {
  return (
    <div class="flex min-w-0 flex-col gap-3" role="status" aria-label={label}>
      <div class="skeleton rounded-box h-28 w-full" aria-hidden="true" />
      <div class="skeleton rounded-box h-64 w-full" aria-hidden="true" />
      <span class="sr-only">Loading…</span>
    </div>
  );
}

function Err({ message, onRetry }: { message: string; onRetry?: () => void }) {
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <div class="alert alert-error alert-soft min-w-0" role="alert">
        <span class="min-w-0 break-words">Couldn't load this view: {message}</span>
      </div>
      {onRetry && (
        <div class="min-w-0">
          <button type="button" class="btn btn-sm" onclick={onRetry}>
            Retry
          </button>
        </div>
      )}
    </div>
  );
}

function Empty({ title, children }: { title: string; children?: unknown }) {
  return (
    <section class="card card-border bg-base-100 min-w-0" role="status" aria-label={title}>
      <div class="card-body items-center gap-2 py-8 text-center">
        <span class="bg-base-200 flex h-11 w-11 items-center justify-center rounded-full opacity-70" aria-hidden="true">
          <DeploysIcon />
        </span>
        <h3 class="m-0 min-w-0 text-base font-semibold">{title}</h3>
        {children && <p class="m-0 min-w-0 text-sm opacity-60">{children}</p>}
      </div>
    </section>
  );
}

export function fmtSize(size: number): string {
  if (size < 1024) {
    return `${size} B`;
  }
  if (size < 1024 * 1024) {
    return `${(size / 1024).toFixed(1)} KiB`;
  }
  return `${(size / (1024 * 1024)).toFixed(1)} MiB`;
}

function statusBadge(status: string): string {
  if (status === "A") {
    return "badge-success";
  }
  if (status === "D") {
    return "badge-error";
  }
  if (status === "M") {
    return "badge-warning";
  }
  return "badge-neutral";
}

// --- File tree ---------------------------------------------------------------

interface TreeNode {
  name: string;
  path: string;
  size: number | null;
  children: TreeNode[];
}

function buildTree(files: { path: string; size: number }[]): TreeNode[] {
  const root: TreeNode[] = [];
  for (const file of files) {
    const parts = file.path.split("/");
    let level = root;
    let prefix = "";
    for (let i = 0; i < parts.length; i++) {
      prefix = prefix ? `${prefix}/${parts[i]}` : parts[i];
      const last = i === parts.length - 1;
      let node = level.find((n) => n.name === parts[i] && (n.size === null) !== last);
      if (!node) {
        node = { name: parts[i], path: prefix, size: last ? file.size : null, children: [] };
        level.push(node);
      }
      if (last) {
        node.size = file.size;
      } else {
        level = node.children;
      }
    }
  }
  const sort = (nodes: TreeNode[]): void => {
    nodes.sort((a, b) => {
      const dir = (b.size === null ? 1 : 0) - (a.size === null ? 1 : 0);
      return dir !== 0 ? dir : a.name.localeCompare(b.name);
    });
    for (const node of nodes) {
      sort(node.children);
    }
  };
  sort(root);
  return root;
}

function TreeNodes({
  nodes,
  depth,
  selected,
  onSelect,
}: {
  nodes: TreeNode[];
  depth: number;
  selected: string | null;
  onSelect: (path: string) => void;
}) {
  return (
    <ul class={`m-0 min-w-0 list-none p-0${depth > 0 ? " pl-3" : ""}`}>
      {nodes.map((node) =>
        node.size === null ? (
          <li key={node.path} class="min-w-0">
            <details open={depth < 2} class="min-w-0">
              <summary
                class="min-w-0 cursor-pointer truncate rounded-field px-2 py-1 text-[13px] font-medium hover:bg-base-200"
                title={node.path}
              >
                {node.name}/
              </summary>
              <TreeNodes nodes={node.children} depth={depth + 1} selected={selected} onSelect={onSelect} />
            </details>
          </li>
        ) : (
          <li key={node.path} class="min-w-0">
            <button
              type="button"
              class={`flex w-full min-w-0 items-center gap-2 rounded-field px-2 py-1 text-left text-[13px] hover:bg-base-200${selected === node.path ? " bg-base-200 font-semibold" : ""}`}
              title={`${node.path} (${fmtSize(node.size)})`}
              onclick={() => onSelect(node.path)}
            >
              <span class="min-w-0 flex-1 truncate font-mono">{node.name}</span>
              <span class="shrink-0 text-xs opacity-50">{fmtSize(node.size)}</span>
            </button>
          </li>
        ),
      )}
    </ul>
  );
}

// --- Unified diff --------------------------------------------------------------

function splitSections(patch: string): { path: string; lines: string[] }[] {
  const sections: { path: string; lines: string[] }[] = [];
  let current: { path: string; lines: string[] } | null = null;
  for (const line of patch.split("\n")) {
    if (line.startsWith("diff --git ")) {
      const rest = line.slice("diff --git ".length).split(" ");
      const raw = rest.length > 1 ? rest[1].slice(2) : rest[0];
      current = { path: raw.replace(/^"|"$/g, ""), lines: [] };
      sections.push(current);
    } else if (current) {
      current.lines.push(line);
    }
  }
  return sections;
}

function diffLineClass(line: string): string {
  if (line.startsWith("+") && !line.startsWith("+++")) {
    return "bg-success/10";
  }
  if (line.startsWith("-") && !line.startsWith("---")) {
    return "bg-error/10";
  }
  if (line.startsWith("@@")) {
    return "opacity-60";
  }
  return "";
}

function DiffSection({ path, lines }: { path: string; lines: string[] }) {
  const big = lines.length > 120;
  const body = (
    <ol class="log-list m-0" aria-label={`Patch for ${path}`}>
      {lines.map((line, i) => (
        <li key={i} class={`log-line ${diffLineClass(line)}`}>
          <code>{line === "" ? " " : line}</code>
        </li>
      ))}
    </ol>
  );
  if (!big) {
    return body;
  }
  return (
    <details class="min-w-0">
      <summary class="btn btn-sm mb-2" title={`Show all ${lines.length} patch lines for ${path}`}>
        Show all {lines.length} lines
      </summary>
      {body}
    </details>
  );
}

// --- The view ------------------------------------------------------------------

export function CodeView({
  slug,
  deploys,
  initialSha,
  initialTab,
  onPickSha,
  onSwitchTab,
}: {
  slug: string;
  deploys: ConsoleDeploy[];
  initialSha: string | null;
  initialTab: "files" | "changes";
  onPickSha: (sha: string | null) => void;
  onSwitchTab: (tab: "files" | "changes") => void;
}) {
  const tree = atom<Panel<TreeResult>>({ kind: "loading" });
  const history = atom<Panel<HistoryResult>>({ kind: "loading" });
  const filePath = atom<string | null>(null);
  const file = atom<Panel<FileResult>>({ kind: "loading" });
  const wrap = atom(true);
  const diff = atom<Panel<DiffResult>>({ kind: "loading" });
  const diffSha = atom<string | null>(null);
  const compareOn = atom(false);
  const compared = atom<Panel<CompareResult>>({ kind: "loading" });

  // One generation per version: late responses for a previous sha never
  // repaint the new one.
  let epoch = 0;

  function liveSha(): string | null {
    const h = history();
    return h.kind === "ready" ? h.value.live_sha : null;
  }
  function currentSha(): string | null {
    const t = tree();
    if (t.kind === "ready") {
      return t.value.sha;
    }
    return initialSha;
  }

  async function loadHistory(): Promise<void> {
    const my = epoch;
    try {
      const value = (await historyAction.run({ slug })) as HistoryResult;
      if (epoch === my) {
        history.set({ kind: "ready", value });
      }
    } catch (err: unknown) {
      if (epoch === my) {
        history.set({ kind: "error", message: String(err) });
      }
    }
  }
  async function loadTree(sha: string | null): Promise<void> {
    const my = epoch;
    tree.set({ kind: "loading" });
    try {
      const value = (await treeAction.run({ slug, sha: sha ?? undefined })) as TreeResult;
      if (epoch !== my) {
        return;
      }
      tree.set({ kind: "ready", value });
      // Preselect the first file the viewer can actually show (at or
      // under the server's 256 KiB blob cap, see `MAX_READ_BYTES` in
      // `git/local.rs`) so the viewer is never an empty well.
      const first = value.files.find((f) => f.size <= 256 * 1024) ?? value.files[0];
      if (!filePath() && first) {
        void openFile(first.path, value.sha);
      }
    } catch (err: unknown) {
      if (epoch === my) {
        tree.set({ kind: "error", message: String(err) });
      }
    }
  }


  async function openFile(path: string, sha: string | null): Promise<void> {
    filePath.set(path);
    file.set({ kind: "loading" });
    const my = epoch;
    try {
      const value = (await fileAction.run({ slug, path, sha: sha ?? undefined })) as FileResult;
      if (epoch === my) {
        file.set({ kind: "ready", value });
      }
    } catch (err: unknown) {
      if (epoch === my) {
        file.set({ kind: "error", message: String(err) });
      }
    }
  }

  async function loadDiff(sha: string | null): Promise<void> {
    if (!sha) {
      return;
    }
    const my = epoch;
    diff.set({ kind: "loading" });
    try {
      const value = (await diffAction.run({ slug, sha })) as DiffResult;
      if (epoch === my) {
        diff.set({ kind: "ready", value });
      }
    } catch (err: unknown) {
      if (epoch === my) {
        diff.set({ kind: "error", message: String(err) });
      }
    }
  }

  async function loadCompare(from: string, to: string): Promise<void> {
    const my = epoch;
    compared.set({ kind: "loading" });
    try {
      const value = (await compareAction.run({ slug, from, to })) as CompareResult;
      if (epoch === my) {
        compared.set({ kind: "ready", value });
      }
    } catch (err: unknown) {
      if (epoch === my) {
        compared.set({ kind: "error", message: String(err) });
      }
    }
  }

  function reload(): void {
    epoch += 1;
    void loadHistory();
    void loadTree(initialSha);
    if (initialTab === "changes") {
      void loadDiff(initialSha ?? liveSha());
    }
  }

  function switchTab(next: "files" | "changes"): void {
    if (next === initialTab) {
      return;
    }
    onSwitchTab(next);
  }

  function toggleCompare(): void {
    const next = !compareOn();
    compareOn.set(next);
    if (next) {
      const from = currentSha();
      const to = liveSha();
      if (from && to && from !== to) {
        void loadCompare(from, to);
      }
    }
  }

  // Boot once per mount; the shell remounts CodeView per sha (keyed), so
  // each version loads its own data exactly once.
  watch.once(() => {
    epoch += 1;
    void loadHistory();
    void loadTree(initialSha);
  });

  const historyPanel = history();
  const treePanel = tree();
  const sha = currentSha();
  const live = liveSha();
  // The diff loads when the changes tab shows, however it was reached:
  // tab click (route update, no remount) or the Deploys deep link onto an
  // already-mounted version. The `diffSha` marker keeps it to one load per
  // version; the epoch inside `loadDiff` drops stale arrivals.
  if (initialTab === "changes" && sha && diffSha() !== sha) {
    diffSha.set(sha);
    void loadDiff(sha);
  }
  const deployBySha = new Map(deploys.map((d) => [d.sha, d]));

  if (historyPanel.kind === "ready" && historyPanel.value.commits.length === 0) {
    return <Empty title="No code yet">Commits your agent ships will show up here.</Empty>;
  }

  const commits = historyPanel.kind === "ready" ? historyPanel.value.commits : [];
  const pickerValue = sha ?? "";

  return (
    <div class="flex min-w-0 flex-col gap-3">
      <div class="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-center" aria-label="Version">
        <label class="form-control min-w-0 flex-1" for="code-version">
          <span class="label min-w-0">
            <span class="label-text truncate" title="The repo version to browse (recent commits with their deploy status)">
              Version{live && sha === live ? " · live" : ""}
            </span>
          </span>
          {historyPanel.kind === "loading" && <div class="skeleton h-9 w-full" aria-hidden="true" />}
          {historyPanel.kind === "error" && (
            <p class="m-0 min-w-0 break-words text-sm text-error" role="alert">
              Couldn't load versions: {historyPanel.message}{" "}
              <button type="button" class="btn btn-xs" onclick={() => void loadHistory()}>
                Retry
              </button>
            </p>
          )}
          {historyPanel.kind === "ready" && (
            <select
              id="code-version"
              class="select select-bordered select-sm w-full min-w-0"
              value={pickerValue}
              onchange={(e: Event) => {
                const value = (e.target as HTMLSelectElement).value;
                onPickSha(value === live ? null : value);
              }}
            >
              {commits.map((c) => {
                const deploy = deployBySha.get(c.sha);
                const marks = [
                  c.sha === live ? "live" : null,
                  deploy ? deploy.status : null,
                ].filter(Boolean);
                const label = `${shortSha(c.sha)} · ${c.message.split("\n")[0] || "(no message)"} · ${timeAgo(c.at * 1000)}${marks.length > 0 ? ` · ${marks.join(", ")}` : ""}`;
                return (
                  <option key={c.sha} value={c.sha} title={`${c.sha}\n${c.author} <${c.email}>\n${when(c.at * 1000)}\n\n${c.message}`}>
                    {label}
                  </option>
                );
              })}
            </select>
          )}
        </label>
        <div class="tabs tabs-box shrink-0" role="tablist" aria-label="Code sections">
          {(["files", "changes"] as const).map((id) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={initialTab === id}
              class={`tab shrink-0 capitalize${initialTab === id ? " tab-active" : ""}`}
              onclick={() => switchTab(id)}
            >
              {id}
            </button>
          ))}
        </div>
      </div>
      {initialTab === "changes" && live && sha && sha !== live && (
        <label class="label min-w-0 cursor-pointer justify-start gap-2">
          <input
            type="checkbox"
            class="checkbox checkbox-sm shrink-0"
            checked={compareOn()}
            onchange={() => toggleCompare()}
          />
          <span class="label-text min-w-0 truncate" title={`Diff this version against the live sha ${shortSha(live)}`}>
            Compare with live ({shortSha(live)})
          </span>
        </label>
      )}
      <div class="border-base-300 border-t" aria-hidden="true" />
      {initialTab === "files" ? (
        <FilesTab
          tree={treePanel}
          file={file()}
          filePath={filePath()}
          wrap={wrap()}
          onToggleWrap={() => wrap.set(!wrap())}
          onSelect={(path) => void openFile(path, sha)}
          onRetry={reload}
        />
      ) : (
        <ChangesTab
          diff={diff()}
          compared={compared()}
          compareOn={compareOn() && !!live && !!sha && sha !== live}
          live={live}
          onOpenVersion={(next) => onPickSha(next === live ? null : next)}
          onRetry={() => void loadDiff(sha)}
        />
      )}
    </div>
  );
}

function FilesTab({
  tree,
  file,
  filePath,
  wrap,
  onToggleWrap,
  onSelect,
  onRetry,
}: {
  tree: Panel<TreeResult>;
  file: Panel<FileResult>;
  filePath: string | null;
  wrap: boolean;
  onToggleWrap: () => void;
  onSelect: (path: string) => void;
  onRetry: () => void;
}) {
  if (tree.kind === "loading") {
    return <Loading label="Loading files" />;
  }
  if (tree.kind === "error") {
    return <Err message={tree.message} onRetry={onRetry} />;
  }
  const nodes = buildTree(tree.value.files);
  return (
    <div class="flex min-w-0 flex-col gap-3">
      {tree.value.truncated && (
        <p class="alert alert-warning alert-soft m-0 min-w-0 text-[13px]" role="note">
          <span class="min-w-0 break-words">
            Listing cut at {tree.value.files.length.toLocaleString()} files — narrow it with a version that touches fewer paths.
          </span>
        </p>
      )}
      <div class="grid min-w-0 grid-cols-1 gap-4 lg:grid-cols-3">
        <div class="min-w-0 lg:col-span-1" aria-label="File tree">
          <h2 class="m-0 min-w-0 truncate text-base font-semibold" title={`${tree.value.files.length.toLocaleString()} files at ${tree.value.sha}`}>
            Files{" "}
            <span class="opacity-60">({tree.value.files.length.toLocaleString()})</span>
          </h2>
          {nodes.length === 0 ? (
            <p class="m-0 min-w-0 text-sm opacity-60">This version has no files.</p>
          ) : (
            <div class="code-fill-tree max-h-[480px] min-w-0 overflow-auto">
              <TreeNodes nodes={nodes} depth={0} selected={filePath} onSelect={onSelect} />
            </div>
          )}
        </div>
        <div class="min-w-0 lg:col-span-2 lg:border-l lg:border-base-300 lg:pl-4">
          <FileViewer file={file} filePath={filePath} wrap={wrap} onToggleWrap={onToggleWrap} onRetry={onRetry} />
        </div>
      </div>
    </div>
  );
}

function FileViewer({
  file,
  filePath,
  wrap,
  onToggleWrap,
  onRetry,
}: {
  file: Panel<FileResult>;
  filePath: string | null;
  wrap: boolean;
  onToggleWrap: () => void;
  onRetry: () => void;
}) {
  if (!filePath) {
    return (
      <p class="m-0 min-w-0 text-sm opacity-60" aria-label="File viewer">
        Pick a file to read it.
      </p>
    );
  }
  if (file.kind === "loading") {
    return <Loading label="Loading file" />;
  }
  if (file.kind === "error") {
    return <Err message={file.message} onRetry={onRetry} />;
  }
  const value = file.value;
  const lines = value.text?.split("\n") ?? [];
  // A trailing newline leaves one empty phantom line; drop it so line
  // numbers match editors.
  if (lines.length > 0 && lines[lines.length - 1] === "") {
    lines.pop();
  }
  return (
    <div class="flex min-w-0 flex-col gap-2" aria-label={`File ${value.path}`}>
      <div class="min-w-0">
        <div class="flex min-w-0 flex-wrap items-center gap-2">
          <h2 class="m-0 min-w-0 flex-1 truncate font-mono text-sm font-semibold" title={value.path}>
            {value.path}
          </h2>
          {value.kind === "text" && <CopyButton value={value.text ?? ""} label="Copy file contents" />}
          {value.kind === "text" && value.text && (
            <button
              type="button"
              class="btn btn-sm shrink-0"
              title="Download this version of the file"
              onclick={() => downloadFile(value.path, value.text ?? "")}
            >
              Download
            </button>
          )}
          <label class="label shrink-0 cursor-pointer gap-2" title="Wrap long lines">
            <span class="label-text text-xs">Wrap</span>
            <input type="checkbox" class="checkbox checkbox-sm" checked={wrap} onchange={onToggleWrap} />
          </label>
        </div>
        <p class="m-0 min-w-0 truncate text-xs opacity-60" title={`SHA ${value.sha}`}>
          {shortSha(value.sha)} · {fmtSize(value.size)}
          {value.total_lines !== undefined && ` · ${value.total_lines.toLocaleString()} lines`}
        </p>
        {value.kind === "binary" && (
          <p class="alert alert-soft m-0 min-w-0 text-[13px]" role="note">
            <span class="min-w-0 break-words">
              Binary file ({fmtSize(value.size)}) — not shown. Only the type and size leave the server.
            </span>
          </p>
        )}
        {value.kind === "too_large" && (
          <p class="alert alert-warning alert-soft m-0 min-w-0 text-[13px]" role="note">
            <span class="min-w-0 break-words">
              {value.path} is {fmtSize(value.size)}, over the 256 KiB read cap — not shown. Pick a smaller file or an earlier version.
            </span>
          </p>
        )}
        {value.kind === "text" && (
          <div class="min-w-0">
            {value.truncated && (
              <p class="alert alert-warning alert-soft m-0 mb-2 min-w-0 text-[13px]" role="note">
                <span class="min-w-0 break-words">
                  Shown text cut at 64 KiB of {fmtSize(value.size)} — the rest stays on the server.
                </span>
              </p>
            )}
            <ol class="log-list code-fill-lines m-0" aria-label={`Contents of ${value.path}`}>
              {lines.map((line, i) => (
                <li key={i} class={`log-line${wrap ? "" : " is-nowrap"}`}>
                  <span class="log-stream w-10 shrink-0 text-right" aria-hidden="true">
                    {i + 1}
                  </span>
                  <code>{line === "" ? " " : line}</code>
                </li>
              ))}
            </ol>
          </div>
        )}
      </div>
    </div>
  );
}

function downloadFile(path: string, text: string): void {
  const blob = new Blob([text], { type: "text/plain" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = path.split("/").pop() ?? "file";
  document.body.appendChild(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

function ChangesTab({
  diff,
  compared,
  compareOn,
  live,
  onOpenVersion,
  onRetry,
}: {
  diff: Panel<DiffResult>;
  compared: Panel<CompareResult>;
  compareOn: boolean;
  live: string | null;
  onOpenVersion: (sha: string) => void;
  onRetry: () => void;
}) {
  if (diff.kind === "loading") {
    return <Loading label="Loading changes" />;
  }
  if (diff.kind === "error") {
    return <Err message={diff.message} onRetry={onRetry} />;
  }
  const value = diff.value;
  const shown: { files: DiffFile[]; patch: string; truncated: boolean } = compareOn
    ? compared.kind === "ready"
      ? compared.value
      : { files: [], patch: "", truncated: false }
      : value;
  const sections = splitSections(shown.patch);
  const sectionByPath = new Map(sections.map((s, i) => [s.path, i]));
  return (
    <div class="flex min-w-0 flex-col gap-4">
      <div class="flex min-w-0 flex-col gap-1 text-[13px]" aria-label="Commit">
        <div class="flex min-w-0 items-center gap-2">
          <span class="min-w-0 flex-1 truncate font-mono" title={value.commit.sha}>
            {shortSha(value.commit.sha)}
          </span>
          <CopyButton value={value.commit.sha} label="Copy full SHA" />
        </div>
        <p class="m-0 min-w-0 break-words font-medium" title={value.commit.message}>
          {value.commit.message || "(no message)"}
        </p>
        <p class="m-0 min-w-0 truncate text-xs opacity-60" title={`${value.commit.author} <${value.commit.email}> · ${when(value.commit.at * 1000)}`}>
          {value.commit.author} · {timeAgo(value.commit.at * 1000)}
        </p>
        <p class="m-0 min-w-0 truncate text-xs opacity-60" title={value.parent ? `Diff vs parent ${value.parent}` : "Initial commit: diff vs the empty tree"}>
          {value.parent ? (
            <span>
              vs parent{" "}
              <button type="button" class="link font-mono" title={value.parent} onclick={() => onOpenVersion(value.parent as string)}>
                {shortSha(value.parent)}
              </button>
            </span>
          ) : (
            "Initial commit"
          )}
        </p>
      </div>
      {compareOn && (
        <div class="min-w-0 text-[13px]" aria-label="Compare with live">
          {compared.kind === "loading" && <div class="skeleton h-20 w-full" aria-hidden="true" />}
          {compared.kind === "error" && (
            <p class="m-0 min-w-0 break-words text-error" role="alert">
              Couldn't compare: {compared.message}
            </p>
          )}
          {compared.kind === "ready" && (
            <p class="m-0 min-w-0 break-words" title={`Diff ${compared.value.from} against live ${compared.value.to}`}>
              {compared.value.from === compared.value.to
                ? "This version is live — nothing differs."
                : `Diff against live (${shortSha(compared.value.to)}): ${compared.value.files.length} file(s).`}
            </p>
          )}
        </div>
      )}
      <div class="min-w-0 border-t border-base-300 pt-3" aria-label="Changed files">
        <h2 class="m-0 min-w-0 text-base font-semibold" title={`${shown.files.length} changed files`}>
          Changed files{" "}
          <span class="opacity-60">({shown.files.length})</span>
        </h2>
        {shown.files.length === 0 ? (
          <p class="m-0 min-w-0 text-sm opacity-60">
            {compareOn ? "Still comparing…" : "No changes in this range."}
          </p>
        ) : (
          <ul class="list min-w-0">
            {shown.files.map((f) => (
              <li key={f.path} class="list-row min-w-0 items-center gap-2 text-[13px]">
                <span class={`badge badge-soft ${statusBadge(f.status)} shrink-0`} title={f.status === "A" ? "Added" : f.status === "D" ? "Deleted" : f.status === "M" ? "Modified" : `Status ${f.status}`}>
                  {f.status}
                </span>
                <a
                  class="min-w-0 flex-1 truncate font-mono"
                  href={`#code-diff-${sectionByPath.get(f.path.replace(/^[ab]\//, "")) ?? 0}`}
                  title={f.path}
                >
                  {f.path}
                </a>
                <span class="shrink-0 whitespace-nowrap font-mono text-xs opacity-60" title={`${f.added} added, ${f.removed} removed`}>
                  {f.binary ? "binary" : `+${f.added} −${f.removed}`}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
      {sections.map((section, i) => (
        <div
          key={`${section.path}-${i}`}
          id={`code-diff-${i}`}
          class="min-w-0 scroll-mt-4 border-t border-base-300 pt-3"
          aria-label={`Patch for ${section.path}`}
        >
          <h3 class="m-0 min-w-0 truncate font-mono text-sm font-semibold" title={section.path}>
            {section.path}
          </h3>
          <DiffSection path={section.path} lines={section.lines} />
        </div>
      ))}
      {shown.truncated && (
        <p class="alert alert-warning alert-soft m-0 min-w-0 text-[13px]" role="note">
          <span class="min-w-0 break-words">Patch cut at 64 KiB — the remaining files stay on the server.</span>
        </p>
      )}
    </div>
  );
}
