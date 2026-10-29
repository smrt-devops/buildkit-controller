'use strict';
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const gha = require('./gha');
const L = require('./lib');

function docker(args) {
  const r = spawnSync('docker', args, { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`docker ${args.slice(0, 2).join(' ')} failed: ${(r.stderr || r.error || '').toString().trim()}`);
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

  const lr = await L.requestWithRetry(`${api}/v1/pools`, { ca, timeoutMs: 30000 },
    { what: 'list pools', getToken, budgetMs: L.SHORT_BUDGET_MS, idempotent: true, log: gha.info });
  if (lr.status !== 200) throw L.apiError('list pools', lr);
  const found = L.validatePools(JSON.parse(lr.body), pools);

  const keys = pools.map(() => L.generateKey());
  keys.forEach((k) => gha.setSecret(k.privateKey));
  // Persist state before any POST so a cancel mid-way still releases what succeeded.
  gha.saveState('api', api);
  gha.saveState('audience', aud);
  gha.saveState('ca', ca || '');
  const root = process.env.RUNNER_TEMP || require('node:os').tmpdir();
  // Every id that may still hold a worker, bound or queued. State is rewritten on each change, so a
  // cancel while an allocation is still queued releases it in the post step.
  const live = new Set();
  const saveIds = () => {
    const ids = [...live];
    gha.saveState('ids', ids.join(','));
    gha.saveState('dirs', ids.map((id) => path.join(root, `buildkit-${id}`)).join(','));
  };
  const results = await Promise.allSettled(pools.map(async (p, i) => {
    // Only a 503 with Retry-After or a failure before the request was sent is retried: anything
    // else may have claimed a worker whose id we would never learn. A 202 hands out the id, which
    // is polled until bound; a queue timeout that runs out on the poll starts a fresh POST.
    // A retry queues only for what is left of the budget, so the total stays near the queue timeout
    // + 60 s. Without queue-timeout, the pool default from GET /v1/pools sets the budget.
    const pqt = L.effectiveQueueTimeout(qt, found[i]);
    const attemptOpts = ({ remainingMs }) => {
      const q = L.queueTimeoutFor(pqt, remainingMs);
      return {
        method: 'POST', ca, timeoutMs: L.postTimeoutMs(remainingMs), headers: { 'Content-Type': 'application/json' },
        body: L.allocationBody(p, keys[i].publicKey, ttl, q, cache),
      };
    };
    const a = await L.allocate(api, {
      what: `allocate ${p.namespace}/${p.pool}`, ca, getToken, attemptOpts, budgetMs: L.retryBudgetMs(pqt), log: gha.info,
      onId: (id) => { live.add(id); saveIds(); },
      onGone: (id) => { live.delete(id); saveIds(); },
    });
    L.checkAllocationId(a.id);
    live.add(a.id);
    saveIds();
    return a;
  }));
  const allocs = results.map((r) => (r.status === 'fulfilled' ? r.value : undefined));
  const failed = results.find((r) => r.status === 'rejected');
  if (failed) {
    await Promise.all([...live].map((id) => L.releaseAllocation({ api, ca, getToken, id, log: gha.info })
      .catch((e) => gha.warning(e.message))));
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
  for (const a of allocs) if (a.cache && a.cache.registry && !a.cache.registry.to) gha.info(`${a.namespace}/${a.pool}: cache key ${a.cache.key}, read only for this build`);
  gha.setOutput('platforms', [...new Set(nodes.flatMap((n) => n.platforms))].join(','));
  gha.info(`builder ${builder} ready with ${ids.length} node(s)`);
}

run().catch((e) => {
  gha.error(e.message);
  process.exitCode = 1;
});
