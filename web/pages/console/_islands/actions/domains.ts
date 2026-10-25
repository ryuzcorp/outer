// Console action registry for custom domains (Pro, spec §7.7): every
// custom-domain query or mutation the Settings card performs goes through
// one exported entry here, so the later ⌘K and WebMCP layer can import
// `actions/*.ts` wholesale. The Settings UI calls `run`.
import { request } from "./http.ts";
import type { ActionContext, ConsoleAction } from "./settings.ts";
import type { DomainEntry, DomainResult, DomainsSummary } from "../api.ts";

function domainPath(slug: string, rest = ""): string {
  return `/api/console/apps/${encodeURIComponent(slug)}/domains${rest}`;
}

const slugSchema = {
  type: "object",
  properties: { app: { type: "string", description: "The app slug." } },
  required: ["app"],
};

export interface ListDomainsInput {
  app: string;
}

export interface AddDomainInput {
  app: string;
  hostname: string;
}

export interface AddDomainOutput {
  domain: DomainEntry;
}

export interface RemoveDomainOutput {
  removed: string;
}

const listDomains: ConsoleAction<ListDomainsInput, DomainsSummary> = {
  name: "list_domains",
  title: "List custom domain",
  description:
    "Show the app's custom domain (at most one): hostname, live verification status, the CNAME target and TXT records to create, and your plan. Refresh with refresh_domains for the latest Cloudflare state.",
  inputSchema: slugSchema,
  readOnly: true,
  consequential: false,
  untrusted: false,
  run(input, _ctx?: ActionContext, signal?: AbortSignal): Promise<DomainsSummary> {
    return request<DomainsSummary>("GET", domainPath(input.app), undefined, signal);
  },
};

const refreshDomains: ConsoleAction<ListDomainsInput, DomainResult> = {
  name: "refresh_domains",
  title: "Refresh custom domain",
  description:
    "Re-read the app's custom domain from Cloudflare and update its stored status (pending, active, or failed with a reason). Use after creating the DNS records.",
  inputSchema: slugSchema,
  readOnly: false,
  consequential: false,
  untrusted: false,
  run(input, _ctx?: ActionContext, signal?: AbortSignal): Promise<DomainResult> {
    return request<DomainResult>("POST", domainPath(input.app, "/refresh"), {}, signal);
  },
};

const addDomain: ConsoleAction<AddDomainInput, AddDomainOutput> = {
  name: "add_domain",
  title: "Add custom domain",
  description:
    "Register a custom hostname for the app (Pro, one per app) as a Cloudflare custom hostname and return the DNS records to create: CNAME hostname to the target, plus TXT records. The domain serves the app once its status turns active.",
  inputSchema: {
    type: "object",
    properties: {
      app: { type: "string", description: "The app slug." },
      hostname: { type: "string", description: "The full domain name, e.g. shop.example.com." },
    },
    required: ["app", "hostname"],
  },
  readOnly: false,
  consequential: false,
  untrusted: false,
  run(input, _ctx?: ActionContext, signal?: AbortSignal): Promise<AddDomainOutput> {
    return request<AddDomainOutput>(
      "POST",
      domainPath(input.app),
      { hostname: input.hostname },
      signal,
    );
  },
};

const removeDomain: ConsoleAction<ListDomainsInput, RemoveDomainOutput> = {
  name: "remove_domain",
  title: "Remove custom domain",
  description:
    "Remove the app's custom domain: deletes the Cloudflare custom hostname, drops the local row, and purges its cached pages. The hostname stops serving the app immediately.",
  inputSchema: slugSchema,
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input, _ctx?: ActionContext, signal?: AbortSignal): Promise<RemoveDomainOutput> {
    return request<RemoveDomainOutput>("DELETE", domainPath(input.app), undefined, signal);
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  listDomains as ConsoleAction<unknown, unknown>,
  refreshDomains as ConsoleAction<unknown, unknown>,
  addDomain as ConsoleAction<unknown, unknown>,
  removeDomain as ConsoleAction<unknown, unknown>,
];

export const domainActions = { listDomains, refreshDomains, addDomain, removeDomain };
