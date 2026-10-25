// R2 listing rows: the list table and the dense columns view.
// Ported from Noite apps/noite/src/lib/storage/r2/rows.tsx @ 58a977a.
// Outer differences: folder navigation is a button (Outer routes by app
// view, not by storage URL, so Noite's Link/useLink have no target),
// icons come from the console set, dates/sizes from data/shared.

import {
  FileIcon,
  FolderIcon,
  ImageIcon,
} from "../../icons.tsx";
import {
  formatAgo,
  formatBytes,
  formatDateTime,
  previewKind,
  type PreviewKind,
} from "../shared.tsx";
import type { R2Folder, R2Object } from "../../actions/data-r2.ts";

function FileTypeIcon({ kind }: { kind: PreviewKind }) {
  if (kind === "image") {
    return <ImageIcon />;
  }

  return <FileIcon />;
}

/** List view: a table of the current folder, folders first, with a checkbox
 * per file and the multi-select bar above it. */
export function R2ListRows({
  canWrite,
  checked,
  files,
  folders,
  onOpen,
  onOpenFolder,
  onToggle,
  onToggleAll,
  selected,
}: {
  canWrite: boolean;
  checked: Set<string>;
  files: R2Object[];
  folders: R2Folder[];
  onOpen: (key: string) => void;
  onOpenFolder: (prefix: string) => void;
  onToggle: (key: string, on: boolean) => void;
  onToggleAll: (on: boolean) => void;
  selected: string | null;
}) {
  const allChecked =
    files.length > 0 && files.every((object) => checked.has(object.key));

  return (
    <div class="min-h-0 flex-1 overflow-auto">
      <table class="table-sm table-pin-rows table w-full">
        <thead>
          <tr>
            {canWrite ? (
              <th class="w-10">
                <input
                  class="checkbox checkbox-sm"
                  type="checkbox"
                  aria-label="Select all files on this page"
                  checked={allChecked}
                  onchange={(event) => {
                    onToggleAll(event.currentTarget.checked);
                  }}
                />
              </th>
            ) : null}
            <th class="whitespace-nowrap">Name</th>
            <th class="whitespace-nowrap">Type</th>
            <th class="whitespace-nowrap">Size</th>
            <th class="whitespace-nowrap">Last modified</th>
          </tr>
        </thead>
        <tbody>
          {folders.map((folder) => (
            <tr key={folder.prefix} class="hover">
              {canWrite ? <td /> : null}
              <td class="max-w-52">
                <button
                  type="button"
                  class="link link-hover inline-flex min-w-0 items-center gap-2"
                  title={folder.prefix}
                  onclick={() => {
                    onOpenFolder(folder.prefix);
                  }}
                >
                  <FolderIcon />
                  <span class="truncate">{folder.name}</span>
                </button>
              </td>
              <td class="text-xs opacity-60">Folder</td>
              <td class="text-xs opacity-60">—</td>
              <td class="text-xs opacity-60">—</td>
            </tr>
          ))}
          {files.map((object) => (
            <tr
              key={object.key}
              class={`hover cursor-pointer ${selected === object.key ? "bg-base-200 dark:bg-base-300/50" : ""}`}
              onclick={() => {
                onOpen(object.key);
              }}
            >
              {canWrite ? (
                <td
                  onclick={(event) => {
                    event.stopPropagation();
                  }}
                >
                  <input
                    class="checkbox checkbox-sm"
                    type="checkbox"
                    aria-label={`Select ${object.name}`}
                    checked={checked.has(object.key)}
                    onchange={(event) => {
                      onToggle(object.key, event.currentTarget.checked);
                    }}
                  />
                </td>
              ) : null}
              <td class="max-w-52">
                <span class="flex min-w-0 items-center gap-2">
                  <FileTypeIcon kind={previewKind(object.contentType)} />
                  <span class="truncate" title={object.key}>
                    {object.name}
                  </span>
                </span>
              </td>
              <td class="max-w-40 truncate text-xs opacity-70" title={object.contentType ?? undefined}>
                {object.contentType ?? "—"}
              </td>
              <td class="text-xs tabular-nums">{formatBytes(object.size)}</td>
              <td
                class="text-xs whitespace-nowrap"
                title={formatDateTime(object.lastModified)}
              >
                {formatAgo(object.lastModified)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** Columns view: the same folder as one dense column beside the detail
 * panel — the compact presentation of a large listing. */
export function R2ColumnRows({
  canWrite,
  checked,
  files,
  folders,
  onOpen,
  onOpenFolder,
  onToggle,
  selected,
}: {
  canWrite: boolean;
  checked: Set<string>;
  files: R2Object[];
  folders: R2Folder[];
  onOpen: (key: string) => void;
  onOpenFolder: (prefix: string) => void;
  onToggle: (key: string, on: boolean) => void;
  selected: string | null;
}) {
  return (
    <div class="min-h-0 flex-1 overflow-auto">
      <ul class="menu m-0 w-full gap-0.5 p-2">
        {folders.map((folder) => (
          <li key={folder.prefix}>
            <button
              type="button"
              class="gap-2"
              title={folder.prefix}
              onclick={() => {
                onOpenFolder(folder.prefix);
              }}
            >
              <FolderIcon />
              <span class="min-w-0 flex-1 truncate">{folder.name}</span>
            </button>
          </li>
        ))}
        {files.map((object) => (
          <li key={object.key}>
            <button
              type="button"
              class={`gap-2 text-left ${selected === object.key ? "menu-active" : ""}`}
              onclick={() => {
                onOpen(object.key);
              }}
            >
              {canWrite ? (
                <input
                  class="checkbox checkbox-sm"
                  type="checkbox"
                  aria-label={`Select ${object.name}`}
                  checked={checked.has(object.key)}
                  onclick={(event) => {
                    event.stopPropagation();
                    onToggle(object.key, event.currentTarget.checked);
                  }}
                />
              ) : null}
              <FileTypeIcon kind={previewKind(object.contentType)} />
              <span class="min-w-0 flex-1 truncate" title={object.key}>
                {object.name}
              </span>
              <span class="shrink-0 text-xs tabular-nums opacity-50">
                {formatBytes(object.size)}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
