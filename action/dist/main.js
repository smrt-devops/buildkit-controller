'use strict';
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const gha = require('./gha');
const L = require('./lib');

function docker(args) {
  const r = spawnSync('docker', args, { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`docker ${args.slice(0, 2).join(' ')} failed: ${(r.stderr || r.error || '').toString().trim()}`);
}

async function release(api, ca, getToken, budgetMs, id) {
  const res = await L.requestWithRetry(`${api}/v1/allocations/${id}`, { method: 'DELETE', ca, timeoutMs: 60000 },
    { what: `release ${id}`, getToken, budgetMs, log: gha.info });
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
  const aud = gha.getInput('audience') || 'buildkit-controller';
  const getToken = () => L.fetchOidcToken(aud, gha.setSecret);
  const budgetMs = L.retryBudgetMs(qt);
  const auth = { Authorization: `Bearer ${await getToken()}` };

  const lr = await L.request(`${api}/v1/pools`, { headers: auth, ca, timeoutMs: 30000 });
  if (lr.status !== 200) throw L.apiError('list pools', lr);
  const found = L.validatePools(JSON.parse(lr.body), pools);

  const keys = pools.map(() => L.generateKey());
  keys.forEach((k) => gha.setSecret(k.privateKey));
  // Persist state before any POST so a cancel mid-way still releases what succeeded.
  gha.saveState('api', api);
  gha.saveState('audience', aud);
  gha.saveState('ca', ca || '');
  gha.saveState('queue-timeout', qt === undefined ? '' : String(qt));
  const root = process.env.RUNNER_TEMP || require('node:os').tmpdir();
  const okIds = [];
  const results = await Promise.allSettled(pools.map(async (p, i) => {
    // Only a 503 with Retry-After or a failure before the request was sent is retried: anything
    // else may have claimed a worker whose id we would never learn.
    const res = await L.requestWithRetry(`${api}/v1/allocations`, {
      method: 'POST', ca, timeoutMs: L.postTimeoutMs(qt), headers: { 'Content-Type': 'application/json' },
      body: L.allocationBody(p, keys[i].publicKey, ttl, qt, cache),
    }, { what: `allocate ${p.namespace}/${p.pool}`, getToken, budgetMs, log: gha.info });
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
    for (const a of allocs) if (a) await release(api, ca, getToken, budgetMs, a.id).catch((e) => gha.warning(e.message));
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
