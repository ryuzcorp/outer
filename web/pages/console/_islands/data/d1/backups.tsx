// D1 Backups tab: the database's stored dumps (time, size), a .sql
// download, a "Back up now" button, and restore behind a consequential
// modal. Lists and mutations run through the registered d1_backup_*
// actions; the download is a plain anchor (the session cookie
// authenticates the GET, same-origin).

import { atom } from "ilha";

import {
  d1run,
  type D1Backup,
  type D1BackupNowInput,
  type D1BackupNowOutput,
  type D1BackupRestoreInput,
  type D1BackupRestoreOutput,
  type D1BackupsInput,
  type D1BackupsOutput,
} from "../../actions/data-d1.ts";
import { ConfirmModal, DataEmptyState, formatBytes, formatDateTime, Toaster, useToasts } from "../shared.tsx";

type BackupsState =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; value: D1Backup[] };

const downloadUrl = (slug: string, binding: string, id: string): string =>
  `/api/console/apps/${encodeURIComponent(slug)}/data/d1/${encodeURIComponent(binding)}/backups/${encodeURIComponent(id)}/download`;

/** Restore confirmation modal: names the backup, warns the live tables are
 * replaced, and notes the fresh backup taken first. The shared
 * `ConfirmModal` (conditionally rendered, no dialog API), with restore
 * wording on the confirm button. */
const RestoreDialog = ({
  backup,
  slug,
  binding,
  onClose,
  onRestored,
}: {
  backup: D1Backup;
  slug: string;
  binding: string;
  onClose: () => void;
  onRestored: (freshId: string) => void;
}) => {
  const busy = atom(false);
  const error = atom("");

  const restore = () => {
    busy.set(true);
    error.set("");
    const input: D1BackupRestoreInput = {
      slug,
      binding,
      backup_id: backup.id,
      confirm: backup.id,
    };
    d1run<D1BackupRestoreInput, D1BackupRestoreOutput>("d1_backup_restore", input).then(
      (value) => {
        onRestored(value.backup.id);
      },
      (failure: unknown) => {
        busy.set(false);
        error.set(failure instanceof Error ? failure.message : String(failure));
      },
    );
  };

  return (
    <>
      <ConfirmModal
        busy={busy()}
        busyLabel="Restoring…"
        confirmLabel="Restore backup"
        title="Restore this backup?"
        consequence={`This replaces the live ${binding} tables with the dump from ${formatDateTime(new Date(backup.created_at))} (${formatBytes(backup.size_bytes)}). A fresh backup is taken first, so a bad restore is itself restorable.`}
        onCancel={onClose}
        onConfirm={restore}
      />
      {error() === "" ? null : (
        <div class="alert alert-error alert-soft text-sm" role="alert">
          <span>{error()}</span>
        </div>
      )}
    </>
  );
};
/** The Backups tab for one binding: dump list, download, back up now, restore. */
export const BackupsPanel = ({
  binding,
  canWrite,
  slug,
}: {
  binding: string;
  canWrite: boolean;
  slug: string;
}) => {
  const state = atom<BackupsState>({ kind: "loading" });
  const generation = atom(0);
  const started = atom("");
  const backingUp = atom(false);
  const restoring = atom<D1Backup | null>(null);
  const { notify: onToast, toasts } = useToasts();

  const listKey = `${slug}:${binding}:${generation()}`;
  if (started() !== listKey) {
    started.set(listKey);
    const input: D1BackupsInput = { slug, binding };
    d1run<D1BackupsInput, D1BackupsOutput>("d1_backups", input).then(
      (value) => {
        if (started() === listKey) {
          state.set({ kind: "ready", value: value.backups });
        }
      },
      (failure: unknown) => {
        if (started() === listKey) {
          state.set({
            kind: "error",
            message: failure instanceof Error ? failure.message : String(failure),
          });
        }
      },
    );
  }

  const refresh = () => {
    generation.set(generation() + 1);
  };

  const backupNow = () => {
    backingUp.set(true);
    const input: D1BackupNowInput = { slug, binding };
    d1run<D1BackupNowInput, D1BackupNowOutput>("d1_backup_now", input).then(
      (value) => {
        backingUp.set(false);
        onToast(`Backed up (${formatBytes(value.backup.size_bytes)}).`);
        refresh();
      },
      (failure: unknown) => {
        backingUp.set(false);
        onToast(failure instanceof Error ? failure.message : String(failure));
      },
    );
  };

  const current = state();

  return (
    <div class="flex min-h-0 flex-col gap-3 p-4">
      <div class="flex flex-wrap items-center gap-2">
        <p class="m-0 text-sm opacity-60">
          Daily dumps, kept 7 days (Pro: 30 days). Downloads are full .sql restores.
        </p>
        {canWrite ? (
          <button
            type="button"
            class="btn btn-sm btn-primary ml-auto"
            disabled={backingUp()}
            onclick={backupNow}
          >
            {backingUp() ? "Backing up…" : "Back up now"}
          </button>
        ) : null}
      </div>
      {current.kind === "error" ? (
        <div class="alert alert-error alert-soft" role="alert">
          <span class="min-w-0">{current.message}</span>
          <button type="button" class="btn btn-sm ml-auto" onclick={refresh}>
            Retry
          </button>
        </div>
      ) : current.kind === "loading" ? (
        <div class="flex min-h-0 flex-1 flex-col gap-2">
          <span class="skeleton h-8 w-full" />
          <span class="skeleton h-24 w-full" />
        </div>
      ) : current.value.length === 0 ? (
        <DataEmptyState
          title="No backups yet"
          hint="The first daily dump hasn't landed, or past dumps aged out. Back up now to take one immediately."
          action={
            canWrite ? (
              <button
                type="button"
                class="btn btn-sm"
                disabled={backingUp()}
                onclick={backupNow}
              >
                {backingUp() ? "Backing up…" : "Back up now"}
              </button>
            ) : undefined
          }
        />
      ) : (
        <div class="overflow-x-auto">
          <table class="table-sm table w-full">
            <thead>
              <tr>
                <th scope="col">Taken</th>
                <th scope="col">Size</th>
                <th scope="col">
                  <span class="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {current.value.map((backup) => (
                <tr key={backup.id}>
                  <td
                    class="whitespace-nowrap tabular-nums"
                    title={new Date(backup.created_at).toISOString()}
                  >
                    {formatDateTime(new Date(backup.created_at))}
                  </td>
                  <td class="whitespace-nowrap tabular-nums">{formatBytes(backup.size_bytes)}</td>
                  <td class="whitespace-nowrap">
                    <div class="flex justify-end gap-2">
                      <a
                        class="btn btn-sm btn-ghost"
                        href={downloadUrl(slug, binding, backup.id)}
                        download
                      >
                        Download
                      </a>
                      {canWrite ? (
                        <button
                          type="button"
                          class="btn btn-sm btn-ghost"
                          onclick={() => restoring.set(backup)}
                        >
                          Restore
                        </button>
                      ) : null}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {restoring() === null || !canWrite ? null : (
        <RestoreDialog
          backup={restoring() as D1Backup}
          slug={slug}
          binding={binding}
          onClose={() => restoring.set(null)}
          onRestored={(freshId) => {
            restoring.set(null);
            onToast(`Restored. A fresh backup was taken first (${freshId.slice(0, 8)}…).`);
            refresh();
          }}
        />
      )}
      <Toaster toasts={toasts()} />
    </div>
  );
};
