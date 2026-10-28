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
      const getToken = () => L.fetchOidcToken(aud, gha.setSecret);
      const qtState = gha.getState('queue-timeout');
      const budgetMs = L.retryBudgetMs(qtState === '' ? undefined : Number(qtState));
      for (const id of ids) {
        try {
          const res = await L.requestWithRetry(`${api}/v1/allocations/${id}`, { method: 'DELETE', ca, timeoutMs: 60000 },
            { what: `release ${id}`, getToken, budgetMs, log: gha.info });
          if (res.status !== 204 && res.status !== 404) throw L.apiError(`release ${id}`, res);
        } catch (e) { failed = true; gha.warning(e.message); }
      }
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
