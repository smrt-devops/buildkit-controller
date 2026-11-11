'use strict';
// The Action's client side for `cargo xtask tunnel-verify` (xtask/src/tunnel_verify.rs), built
// from the shipped action/dist/lib.js the way action/dist/main.js uses it: writeMaterial for each
// allocation, useTunnels to start action/dist/tunnel.js with one local port per tunnel node, and
// the buildx commands of createBuilder. `buildx use` is left out, so the host's current builder
// stays as it was.
//
//   node client.js <action/dist> <config.json>
//
// config.json: {"api", "caFile", "builder", "nodes": [{"alloc", "key", "dir", "platforms"}]}, where
// `alloc` is the allocation response and `key` the private key PEM, both as files. It prints one
// JSON line, {"pid", "endpoints"}, once the builder exists, and stops the forwarder when its stdin
// ends or on SIGTERM or SIGINT.
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const L = require(path.join(path.resolve(process.argv[2]), 'lib.js'));

let pid;
const stop = (code) => {
  if (pid) L.stopForwarder(pid);
  process.exit(code);
};
process.on('SIGTERM', () => stop(0));
process.on('SIGINT', () => stop(0));
process.stdin.on('end', () => stop(0));
process.stdin.resume();

async function main() {
  const cfg = JSON.parse(fs.readFileSync(process.argv[3], 'utf8'));
  const ca = fs.readFileSync(cfg.caFile, 'utf8');
  const direct = cfg.nodes.map((n) => {
    const a = JSON.parse(fs.readFileSync(n.alloc, 'utf8'));
    const m = L.writeMaterial(n.dir, a, fs.readFileSync(n.key, 'utf8'));
    return { ...m, endpoint: a.endpoint, serverName: a.serverName, platforms: n.platforms };
  });
  const nodes = await L.useTunnels(direct, { api: cfg.api, ca, onStarted: (p) => { pid = p; } });
  for (const args of L.buildxCommands(cfg.builder, nodes)) {
    if (args[1] !== 'use') execFileSync('docker', args, { stdio: ['ignore', 'ignore', 'inherit'] });
  }
  process.stdout.write(`${JSON.stringify({ pid, endpoints: nodes.map((n) => n.endpoint) })}\n`);
}

main().catch((e) => {
  process.stderr.write(`${e.stack || e}\n`);
  stop(1);
});
