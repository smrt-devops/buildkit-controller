'use strict';
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const gha = require('./gha');
const L = require('./lib');

async function oidcToken(audience) {
  const base = process.env.ACTIONS_ID_TOKEN_REQUEST_URL;
  const bearer = process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
  if (!base || !bearer) throw new Error('OIDC unavailable: add `permissions: id-token: write` to the job');
  gha.setSecret(bearer);
  const url = `${base}${base.includes('?') ? '&' : '?'}audience=${encodeURIComponent(audience)}`;
  const res = await L.request(url, { headers: { Authorization: `Bearer ${bearer}` }, timeoutMs: 30000 });
  if (res.status !== 200) throw L.apiError('OIDC token request', res);
  const token = JSON.parse(res.body).value;
  if (!token) throw new Error('OIDC token response had no value');
  gha.setSecret(token);
  return token;
}

function docker(args) {
  const r = spawnSync('docker', args, { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`docker ${args.slice(0, 2).join(' ')} failed: ${(r.stderr || r.error || '').toString().trim()}`);
}

async function release(api, ca, token, id) {
  const res = await L.request(`${api}/v1/allocations/${id}`, { method: 'DELETE', headers: { Authorization: `Bearer ${token}` }, ca, timeoutMs: 60000 });
  if (res.status !== 204 && res.status !== 404) throw L.apiError(`release ${id}`, res);
}

async function run() {
  const api = gha.getInput('url').replace(/\/+$/, '');
  const pools = L.parseList(gha.getInput('pools')).map(L.parsePool);
  if (!/^https:\/\//.test(api)) throw new Error('url must be https:// (the OIDC token is sent to it)');
  if (!api || pools.length === 0) throw new Error('url and pools are required');
  const ttl = L.parseDuration(gha.getInput('ttl'));
  const qt = L.parseDuration(gha.getInput('queue-timeout'));
  const ca = gha.getInput('ca-cert') || undefined;
  const cache = L.parseCacheInputs({
    cache: gha.getInput('cache'), cacheKey: gha.getInput('cache-key'),
    cacheMode: gha.getInput('cache-mode'), cacheFrom: gha.getInput('cache-from'),
  });
  const token = await oidcToken(gha.getInput('audience') || 'buildkit-controller');
  const auth = { Authorization: `Bearer ${token}` };

  const lr = await L.request(`${api}/v1/pools`, { headers: auth, ca, timeoutMs: 30000 });
  if (lr.status !== 200) throw L.apiError('list pools', lr);
  const found = L.validatePools(JSON.parse(lr.body), pools);

  const keys = pools.map(() => L.generateKey());
  keys.forEach((k) => gha.setSecret(k.privateKey));
  // Persist state before any POST so a cancel mid-way still releases what succeeded.
  const aud = gha.getInput('audience') || 'buildkit-controller';
  gha.saveState('api', api);
  gha.saveState('audience', aud);
  gha.saveState('ca', ca || '');
  const root = process.env.RUNNER_TEMP || require('node:os').tmpdir();
  const okIds = [];
  const results = await Promise.allSettled(pools.map(async (p, i) => {
    const res = await L.request(`${api}/v1/allocations`, {
      method: 'POST', ca, timeoutMs: L.postTimeoutMs(qt), headers: { ...auth, 'Content-Type': 'application/json' },
      body: L.allocationBody(p, keys[i].publicKey, ttl, qt, cache),
    });
    if (res.status !== 201) throw L.apiError(`allocate ${p.namespace}/${p.pool}`, res);
    const a = JSON.parse(res.body);
    L.checkAllocationId(a.id);
    okIds.push(a.id);
    gha.saveState('ids', okIds.join(','));
    gha.saveState('dirs', okIds.map((id) => path.join(root, `buildkit-${id}`)).join(','));
    return a;
  }));
  const allocs = results.map((r) => (r.status === 'fulfilled' ? r.value : undefined));
  const failed = results.find((r) => r.status === 'rejected');
  if (failed) {
    for (const a of allocs) if (a) await release(api, ca, token, a.id).catch((e) => gha.warning(e.message));
    throw failed.reason;
  }

  const ids = allocs.map((a) => a.id);
  const dirs = ids.map((id) => path.join(root, `buildkit-${id}`));
  const builder = gha.getInput('builder-name') || `bk-${ids[0]}`;

  const nodes = allocs.map((a, i) => {
    const m = L.writeMaterial(dirs[i], a, keys[i].privateKey);
    return { ...m, endpoint: a.endpoint, serverName: a.serverName, platforms: found[i].platforms || [] };
  });
  L.createBuilder(builder, nodes, docker, () => gha.saveState('builder', builder));

  gha.setOutput('builder', builder);
  gha.setOutput('allocation-ids', ids.join(','));
  const flags = L.cacheFlags(allocs, cache);
  gha.setOutput('cache-to', flags.to.join('\n'));
  gha.setOutput('cache-from', flags.from.join('\n'));
  gha.setOutput('cache-args', flags.args.join(' '));
  for (const a of allocs) if (a.cache && a.cache.local) gha.info(`${a.namespace}/${a.pool}: cache key ${a.cache.key}, local ${a.cache.local.result}`);
  gha.setOutput('platforms', [...new Set(nodes.flatMap((n) => n.platforms))].join(','));
  gha.info(`builder ${builder} ready with ${ids.length} node(s)`);
}

run().catch((e) => {
  gha.error(e.message);
  process.exitCode = 1;
});
