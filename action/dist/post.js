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
      const base = process.env.ACTIONS_ID_TOKEN_REQUEST_URL;
      const bearer = process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
      if (!base || !bearer) throw new Error('OIDC unavailable: allocations will drain at their TTL');
      gha.setSecret(bearer);
      const url = `${base}${base.includes('?') ? '&' : '?'}audience=${encodeURIComponent(gha.getState('audience'))}`;
      const tr = await L.request(url, { headers: { Authorization: `Bearer ${bearer}` }, timeoutMs: 30000 });
      if (tr.status !== 200) throw L.apiError('OIDC token request', tr);
      const token = JSON.parse(tr.body).value;
      gha.setSecret(token);
      for (const id of ids) {
        try {
          const res = await L.request(`${api}/v1/allocations/${id}`, { method: 'DELETE', headers: { Authorization: `Bearer ${token}` }, ca, timeoutMs: 60000 });
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
