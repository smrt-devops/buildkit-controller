'use strict';
const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
const gha = require('./gha');
const L = require('./lib');

async function run() {
  const ids = L.parseList(gha.getState('ids'));
  const dirs = L.parseList(gha.getState('dirs'));
  const builder = gha.getState('builder');
  const api = gha.getState('api');
  const ca = gha.getState('ca') || undefined;
  let failed = false;
  try {
    if (builder) spawnSync('docker', ['buildx', 'rm', '--force', builder], { encoding: 'utf8' });
    if (ids.length) {
      const aud = gha.getState('audience');
      const getToken = L.cachedToken(() => L.fetchOidcToken(aud, gha.setSecret));
      // In parallel, each under its own short retry budget, so the step ends within a minute or two.
      await Promise.all(ids.map((id) => L.releaseAllocation({ api, ca, getToken, id, log: gha.info })
        .catch((e) => { failed = true; gha.warning(e.message); })));
    }
  } catch (e) {
    failed = true;
    gha.warning(e.message);
  } finally {
    for (const d of dirs) fs.rmSync(d, { recursive: true, force: true });
  }
  if (failed) gha.warning('some allocations were not released; they drain at expiresAt');
}

run().catch((e) => gha.warning(e.message));
