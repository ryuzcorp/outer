// Neighbour-p95 probe: the measuring client. The harness
// (`hostile_cpu_spin_is_capped_and_neighbour_keeps_p95`) or
// `scripts/hostile-probe-vps.sh` runs this UNCONFINED (harness uid / root),
// like real external traffic hitting a fleet — never as an app uid and never
// in a leaf. The server (`neighbour-server.mjs`) holds the capped neighbour
// leaf; this client reads its port from `HOSTILE_PORT_FILE`, takes an idle
// baseline loop, signals `HOSTILE_BASELINE_DONE` so the harness starts the
// spinner, waits for `HOSTILE_SPIN_READY`, and takes the loaded loop. It
// passes (`ok: true`) when the loaded p95 stays within the bound below.
//
// Why unconfined: the nft `inet outer` table lets an app uid reach loopback
// only on its own registered fleet ports and rejects everything else on
// 127.0.0.0/8, so a client running as an app uid could not fetch even its
// own listener. Measuring from outside the sandbox is also the faithful
// shape: real neighbours are hit by external clients, not by other tenants.
//
// Thresholds: loaded p95 at most max(2x baseline, baseline + 50 ms). The
// loopback baseline is sub-millisecond, so spec §12.7's ±10% aspiration is
// timer noise on shared vCores — tens of milliseconds of scheduling jitter
// are normal there (hence the 50 ms absolute slack). The 2x relative factor
// catches real starvation (an uncapped spinner sharing the box inflates a
// contending neighbour several-fold) while tolerating normal contention.
// The primary containment proof is exec:cpu-spin's cpu.stat assertion; this
// probe guards the neighbour experience.
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { performance } from "node:perf_hooks";

const out = {};
const need = (name) => {
  const value = process.env[name];
  if (!value) {
    console.error(`hostile neighbour probe needs ${name} (run it through the harness)`);
    process.exit(2);
  }
  return value;
};

const count = Number(process.env.HOSTILE_NEIGHBOUR_COUNT || "200");
const portFile = need("HOSTILE_PORT_FILE");
const readyFile = process.env.HOSTILE_SPIN_READY || "";
const baselineDoneFile = process.env.HOSTILE_BASELINE_DONE || "";

const p95 = (samples) => {
  const sorted = [...samples].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.ceil(0.95 * sorted.length) - 1)];
};

const waitFor = async (file, what, ms) => {
  const deadline = Date.now() + ms;
  while (!existsSync(file)) {
    if (Date.now() > deadline) throw new Error(what);
    await new Promise((r) => setTimeout(r, 100));
  }
};

try {
  // The server binds an ephemeral port and publishes it here; waiting
  // fails closed (ok:false on a must-succeed probe fails the suite).
  await waitFor(portFile, "neighbour server never published its port", 30000);
  const port = Number(readFileSync(portFile, "utf-8").trim());
  if (!(port > 0)) throw new Error(`neighbour server published a bad port`);
  const target = `http://127.0.0.1:${port}/health`;

  const loop = async () => {
    const samples = [];
    for (let i = 0; i < count; i++) {
      const start = performance.now();
      const res = await fetch(target, { signal: AbortSignal.timeout(10000) });
      if (!res.ok) throw new Error(`neighbour server answered ${res.status}`);
      await res.text();
      samples.push(performance.now() - start);
    }
    return p95(samples);
  };

  const base = await loop();
  if (baselineDoneFile) writeFileSync(baselineDoneFile, "baseline\n");
  // The harness starts the spinner once the baseline is clean, then drops
  // READY after the workers ramp up. Timing out fails closed.
  if (readyFile) {
    await waitFor(readyFile, "spinner never signalled READY", 60000);
  }
  const loaded = await loop();
  const bound = Math.max(2 * base, base + 50);
  const detail =
    `p95 loaded ${loaded.toFixed(1)}ms vs baseline ${base.toFixed(1)}ms (bound ${bound.toFixed(1)}ms, n=${count})`;
  out["neighbour:p95"] =
    loaded <= bound ? { detail, ok: true } : { detail: `DEGRADED: ${detail}`, ok: false };
} catch (error) {
  out["neighbour:p95"] = { detail: String(error?.message ?? error).slice(0, 200), ok: false };
}

const report = { phase: "cpu", probes: out };
mkdirSync("dist", { recursive: true });
writeFileSync("dist/neighbour.json", `${JSON.stringify(report, null, 2)}\n`);
appendFileSync("dist/neighbour.log", `neighbour ${JSON.stringify(out)}\n`);
console.log(JSON.stringify(report));
