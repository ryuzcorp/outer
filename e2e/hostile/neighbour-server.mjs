// Neighbour /health server: the well-behaved tenant the p95 probe
// measures. The harness (`hostile_cpu_spin_is_capped_and_neighbour_keeps_p95`)
// or `scripts/hostile-probe-vps.sh` runs this in a neighbour leaf with the
// standard fleet limits as an app uid. It binds loopback on an ephemeral
// port (binding is fine; the nft policy only polices packets), writes the
// port to `HOSTILE_PORT_FILE`, and serves until `HOSTILE_STOP_FILE` appears
// (polled every 500 ms) or the harness kills the leaf.
//
// Why a separate server: the nft `inet outer` table lets an app uid reach
// loopback only on its own registered fleet ports and rejects everything
// else on 127.0.0.0/8 — so a probe running AS an app uid cannot fetch its
// own ephemeral listener. The measuring client therefore runs unconfined
// (like real external traffic), while this capped server is the fleet under
// test.
import { createServer } from "node:http";
import { existsSync, writeFileSync } from "node:fs";

const need = (name) => {
  const value = process.env[name];
  if (!value) {
    console.error(`hostile neighbour server needs ${name} (run it through the harness)`);
    process.exit(2);
  }
  return value;
};

const portFile = need("HOSTILE_PORT_FILE");
const stopFile = process.env.HOSTILE_STOP_FILE || "";
const server = createServer((req, res) => {
  if (req.url === "/health") {
    res.writeHead(200, { "content-length": "2", connection: "close" });
    res.end("ok");
  } else {
    res.writeHead(404, { connection: "close" });
    res.end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
writeFileSync(portFile, `${port}\n`);
console.log(`neighbour serving on 127.0.0.1:${port}`);
if (stopFile) {
  while (!existsSync(stopFile)) {
    await new Promise((r) => setTimeout(r, 500));
  }
  server.close();
} else {
  await new Promise(() => {});
}
