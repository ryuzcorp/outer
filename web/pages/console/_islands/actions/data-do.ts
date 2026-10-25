// Durable Objects viewer actions: one registered entry per query the DO
// panel performs. Read-only end to end — celld exposes no generic storage
// read for a DO instance, so the preview is the app's own `?read=1`
// response and stays blank when the fleet is asleep or the object has no
// read handler.

import { request } from "./http.ts";
import type { ConsoleAction } from "./settings.ts";

export interface DoBinding {
  name: string;
  /** The class name behind the binding. */
  class: string;
}

export interface DoInstance {
  id: string;
  scope: string;
}

export interface DoClassesResult {
  classes: DoBinding[];
}

export interface DoInstancesResult {
  class_name: string;
  instances: DoInstance[];
}

export interface DoProbeResult {
  scope: string;
  preview: string | null;
}

const slugSchema = { type: "string", minLength: 1, maxLength: 64 };

function qs(params: Record<string, string | undefined>): string {
  const q = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined) {
      q.set(key, value);
    }
  }
  const text = q.toString();
  return text ? `?${text}` : "";
}

const doListClasses: ConsoleAction<{ slug: string }, DoClassesResult> = {
  name: "do_list_classes",
  title: "List Durable Object classes",
  description:
    "List the app's declared Durable Object bindings (name + class) from its built wrangler config. Use when the DO viewer needs its class picker; an empty list means the app declares no durable_objects.",
  inputSchema: {
    type: "object",
    properties: { slug: slugSchema },
    required: ["slug"],
  },
  readOnly: true,
  consequential: false,
  untrusted: true,
  run: ({ slug }, signal) =>
    request<DoClassesResult>(
      "GET",
      `/api/console/apps/${encodeURIComponent(slug)}/data/do/classes`,
      undefined,
      signal,
    ),
};

const doListInstances: ConsoleAction<{ slug: string; class: string }, DoInstancesResult> = {
  name: "do_list_instances",
  title: "List Durable Object instances",
  description:
    "List the instances of one Durable Object class (id + scope) via `celld cell list`. Reserved celld-internal classes are skipped. Works while the fleet is asleep.",
  inputSchema: {
    type: "object",
    properties: {
      slug: slugSchema,
      class: { type: "string", minLength: 1 },
    },
    required: ["slug", "class"],
  },
  readOnly: true,
  consequential: false,
  untrusted: true,
  run: ({ slug, class: className }, signal) =>
    request<DoInstancesResult>(
      "GET",
      `/api/console/apps/${encodeURIComponent(slug)}/data/do/instances${qs({ class: className })}`,
      undefined,
      signal,
    ),
};

const doProbe: ConsoleAction<{ slug: string; scope: string }, DoProbeResult> = {
  name: "do_probe",
  title: "Preview Durable Object state",
  description:
    "Best-effort preview of one instance's state: GETs the fleet's /do/{scope}?read=1 handler (5 s timeout, truncated). The app opts in by implementing the read-only ?read=1 branch; preview is null when the fleet is asleep or the handler is missing. Never an error.",
  inputSchema: {
    type: "object",
    properties: {
      slug: slugSchema,
      scope: { type: "string", minLength: 1 },
    },
    required: ["slug", "scope"],
  },
  readOnly: true,
  consequential: false,
  untrusted: true,
  run: ({ slug, scope }, signal) =>
    request<DoProbeResult>(
      "GET",
      `/api/console/apps/${encodeURIComponent(slug)}/data/do/probe${qs({ scope })}`,
      undefined,
      signal,
    ),
};

export const actions: ConsoleAction<any, any>[] = [
  doListClasses,
  doListInstances,
  doProbe,
];

export const doActions = {
  listClasses: doListClasses,
  listInstances: doListInstances,
  probe: doProbe,
};
