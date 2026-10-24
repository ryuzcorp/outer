// The `/console` apps list: the account's apps as daisyUI cards with a
// status badge, URL link, last deploy and last request. 401s leave through
// the sign-in flow (see api.ts).
import { atom } from "ilha";
import { watch } from "ilha";

import { api, shortSha, when, type ConsoleApp } from "./api.ts";

type State =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; apps: ConsoleApp[]; email: string };

function badge(status: string): string {
  switch (status) {
    case "running":
      return "badge badge-success";
    case "sleeping":
      return "badge badge-neutral";
    case "building":
      return "badge badge-info";
    case "failed":
      return "badge badge-error";
    default:
      return "badge badge-ghost";
  }
}

async function signOut(): Promise<void> {
  await fetch("/api/console/sign-out", { method: "POST" });
  globalThis.location.assign("/console/sign-in");
}

export function AppsList() {
  const state = atom<State>({ kind: "loading" });
  watch.once(() => {
    if (typeof window === "undefined") {
      return;
    }
    api<{ apps: ConsoleApp[] }>("/api/console/apps")
      .then(async (apps) => {
        // The apps endpoint carries no email; one /me call labels the page.
        const me = await api<{ email: string }>("/api/console/me");
        state.set({ kind: "ready", apps: apps.apps, email: me.email });
      })
      .catch((err: unknown) => {
        state.set({ kind: "error", message: String(err) });
      });
  });

  const current = state();
  if (current.kind === "loading") {
    return (
      <div class="flex justify-center py-10">
        <span class="loading loading-spinner" aria-label="Loading apps" />
      </div>
    );
  }
  if (current.kind === "error") {
    return (
      <div role="alert" class="alert alert-error">
        Couldn't load your apps: {current.message}
      </div>
    );
  }
  return (
    <div class="flex flex-col gap-4">
      <p class="text-sm opacity-70">
        Signed in as {current.email}{" "}
        <button
          type="button"
          class="btn btn-ghost btn-xs"
          onclick={() => void signOut()}
        >
          Sign out
        </button>
      </p>
      {current.apps.length === 0 ? (
        <div role="status" class="alert">
          No apps yet. Ask your agent to create one over MCP.
        </div>
      ) : (
        <ul class="grid gap-4 md:grid-cols-2">
          {current.apps.map((app) => (
            <li
              key={app.slug}
              class="card bg-base-100 shadow-sm border border-base-300"
            >
              <div class="card-body gap-2">
                <div class="flex items-center justify-between gap-2">
                  <h2 class="card-title">{app.slug}</h2>
                  <span class={badge(app.status)}>{app.status}</span>
                </div>
                <a class="link text-sm break-all" href={app.url}>
                  {app.url}
                </a>
                <dl class="text-sm opacity-80">
                  <div class="flex gap-2">
                    <dt>Visibility</dt>
                    <dd>{app.visibility}</dd>
                  </div>
                  <div class="flex gap-2">
                    <dt>Last deploy</dt>
                    <dd>
                      {app.last_deploy
                        ? `${shortSha(app.last_deploy.sha)} · ${app.last_deploy.status}`
                        : "none yet"}
                    </dd>
                  </div>
                  <div class="flex gap-2">
                    <dt>Last request</dt>
                    <dd>{when(app.last_request_at)}</dd>
                  </div>
                </dl>
                <div class="card-actions justify-end">
                  <a
                    class="btn btn-sm"
                    href={`/console/app?app=${encodeURIComponent(app.slug)}`}
                  >
                    Open
                  </a>
                </div>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
