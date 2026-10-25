// One registry for the whole console: the ⌘K palette and the WebMCP
// layer register everything here. Adding a registry later is a one-line
// change (the custom-domains branch adds its own line). Names must be
// unique: a dev-time throw fails fast instead of silently shadowing.
import type { ConsoleAction } from "./settings.ts";

import { actions as accountActions } from "./account.ts";
import { actions as codeActions } from "./code.ts";
import { actions as collaboratorActions } from "./collaborators.ts";
import { actions as consoleActions } from "./console.ts";
import { actions as d1Actions } from "./data-d1.ts";
import { actions as doActions } from "./data-do.ts";
import { actions as r2Actions } from "./data-r2.ts";
import { actions as deployActions } from "./deploys.ts";
import { actions as observeActions } from "./observe.ts";
import { actions as secretsActions } from "./secrets.ts";
import { actions as settingsActions } from "./settings.ts";

export type { ConsoleAction };
export type AnyAction = ConsoleAction<any, any>;

const registries: ConsoleAction<any, any>[][] = [
  consoleActions as ConsoleAction<any, any>[],
  settingsActions as ConsoleAction<any, any>[],
  secretsActions as ConsoleAction<any, any>[],
  deployActions as ConsoleAction<any, any>[],
  d1Actions as ConsoleAction<any, any>[],
  r2Actions as ConsoleAction<any, any>[],
  doActions as ConsoleAction<any, any>[],
  observeActions as ConsoleAction<any, any>[],
  codeActions as ConsoleAction<any, any>[],
  accountActions as ConsoleAction<any, any>[],
  collaboratorActions as ConsoleAction<any, any>[],
];

function combine(): AnyAction[] {
  const seen = new Map<string, number>();
  const out: AnyAction[] = [];
  for (const registry of registries) {
    for (const action of registry) {
      if (seen.has(action.name)) {
        throw new Error(
          `duplicate console action name: ${action.name} (registries ${seen.get(action.name)} and ${registries.indexOf(registry)})`,
        );
      }
      seen.set(action.name, registries.indexOf(registry));
      out.push(action);
    }
  }
  return out;
}

export const allActions: AnyAction[] = combine();

export function findAction(name: string): AnyAction | null {
  return allActions.find((a) => a.name === name) ?? null;
}
