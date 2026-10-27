// Hostile app worker (celld): serves its build/r2 probe results as JSON and
// runs the Worker-side network probes live. Every probe must fail except
// internet egress. Borrowed from Noite's apps/noite/test/hostile/index.js:
// Outer drops the Caddy/runner/control-port targets and aims at Outer's own
// loopback surface instead (neighbour fleet ports, outer's proxy port, the
// celld internal listener). Query: /probes?neighbour=<port>&platform=<port>.
import buildProbes from "./dist/probe.json";
import releaseProbes from "./dist/release-probe.json";

const attempt = async (name, fn) => {
  try {
    const detail = await fn();
    return [name, { detail: String(detail).slice(0, 200), ok: true }];
  } catch (error) {
    return [name, { detail: String(error).slice(0, 200), ok: false }];
  }
};

const timedFetch = (url, ms = 3000) =>
  fetch(url, { signal: AbortSignal.timeout(ms) }).then(
    (res) => `status ${res.status}`
  );

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/probes") {
      const worker = {};
      const neighbour = Number(url.searchParams.get("neighbour") ?? "0");
      const platform = Number(url.searchParams.get("platform") ?? "0");

      // Another fleet's public port: the harness passes a neighbour that is
      // really serving, so a failure proves isolation, not absence.
      const [n1, v1] = await attempt("fetch:neighbour-fleet", () =>
        timedFetch(`http://127.0.0.1:${neighbour}/`)
      );
      worker[n1] = v1;

      // Outer's own loopback surface (proxy port, celld internal listener):
      // unreachable from a tenant uid under the nft policy.
      const [n2, v2] = await attempt("fetch:platform-internal", () =>
        timedFetch(`http://127.0.0.1:${platform}/`)
      );
      worker[n2] = v2;

      const [n3, v3] = await attempt("fetch:metadata", () =>
        timedFetch("http://169.254.169.254/")
      );
      worker[n3] = v3;

      // SMTP is HTTP-probed: any answer at all is a leak, whatever the
      // status. A refused/reset connection throws and stays contained.
      const [n4, v4] = await attempt("fetch:smtp-worker", async () => {
        const results = [];
        for (const port of [25, 465, 587]) {
          try {
            results.push(`${port}=${await timedFetch(`http://127.0.0.1:${port}/`, 2000)}`);
          } catch (error) {
            results.push(`${port}=refused`);
          }
        }
        if (results.every((r) => r.endsWith("refused"))) {
          throw new Error(results.join(" "));
        }
        return results.join(" ");
      });
      worker[n4] = v4;

      const [n5, v5] = await attempt("fetch:internet", async () => {
        const res = await fetch("https://example.com/", {
          signal: AbortSignal.timeout(5000),
        });
        if (!res.ok) throw new Error(`status ${res.status}`);
        return `status ${res.status}`;
      });
      // Inverted: internet must succeed, so ok means the probe reached it.
      worker[n5] = { detail: v5.detail, mustSucceed: true, ok: v5.ok };

      const secrets = {};
      for (const key of [
        "OUTER_SECRET",
        "AWS_SECRET_ACCESS_KEY",
        "R2_SECRET_ACCESS_KEY",
        "CLOUDFLARE_API_TOKEN",
        "STRIPE_SECRET_KEY",
      ]) {
        if (key === "AWS_SECRET_ACCESS_KEY") {
          // The fleet legitimately holds *scoped* bucket creds under the
          // AWS names; the probe asserts they are not the parent key by
          // checking the session token rides along (temporary credentials).
          const token = env["AWS_SESSION_TOKEN"];
          secrets["env:worker-secrets"] =
            env[key] && !token ? "present-without-token" : "scoped-or-absent";
        } else {
          secrets[key] = env[key] ? "present" : "absent";
        }
      }

      return Response.json({
        build: buildProbes,
        release: releaseProbes,
        secrets,
        worker,
      });
    }
    return new Response("hostile tenant — GET /probes", { status: 404 });
  },
};
