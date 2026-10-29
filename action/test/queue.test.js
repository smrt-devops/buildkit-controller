'use strict';
// allocate() and pollAllocation(): a 202 from POST is polled with GET /v1/allocations/{id}?wait=25s.
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const L = require('../dist/lib');

const ALLOC = { id: 'abc123', endpoint: 'tcp://e:1', serverName: 'e', clientCert: 'C' };
const pending = { status: 202, body: JSON.stringify({ id: 'abc123', state: 'pending', position: 1 }) };
const bound = { status: 200, body: JSON.stringify(ALLOC) };
const err = (status, code, headers) => ({ status, headers, body: JSON.stringify({ error: { code, message: 'x' } }) });

/** Server with one script per method; each request takes the next step (the last repeats). */
async function serve({ post = [], get = [] }) {
  const seen = [];
  const steps = { POST: post, GET: get };
  const count = { POST: 0, GET: 0 };
  const srv = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const raw = Buffer.concat(chunks).toString();
      seen.push({ method: req.method, url: req.url, body: raw ? JSON.parse(raw) : undefined });
      const list = steps[req.method];
      const step = list[Math.min(count[req.method]++, list.length - 1)];
      if (step.reset) return req.socket.destroy();
      res.writeHead(step.status, step.headers || {});
      res.end(step.body ?? '');
    });
  });
  await new Promise((r) => srv.listen(0, '127.0.0.1', r));
  return { api: `http://127.0.0.1:${srv.address().port}`, seen, close: () => srv.close() };
}

/** Fake clock: sleeping advances it. */
function clock() {
  const c = { t: 0, waits: [], logs: [] };
  c.now = () => c.t;
  c.sleep = async (ms) => { c.waits.push(ms); c.t += ms; };
  c.log = (m) => c.logs.push(m);
  return c;
}

const run = (s, c, extra = {}) => {
  const ids = { got: [], gone: [] };
  const p = L.allocate(s.api, {
    what: 'allocate ci/a', getToken: async () => 'tok', budgetMs: L.retryBudgetMs(300), attemptOpts: ({ publicKey }) => ({ method: 'POST', body: { publicKey }, timeoutMs: 5000 }),
    onId: (id) => ids.got.push(id), onGone: (id) => ids.gone.push(id), log: c.log, sleep: c.sleep, now: c.now, ...extra,
  });
  return { p, ids };
};

test('a 201 needs no poll', async () => {
  const s = await serve({ post: [{ status: 201, body: bound.body }] });
  const c = clock();
  const { p, ids } = run(s, c);
  assert.deepEqual(await p, ALLOC);
  s.close();
  assert.deepEqual(s.seen.map((r) => r.method), ['POST']);
  assert.deepEqual(ids.got, []);
});

test('a 202 is polled with wait=25s until the allocation is bound', async () => {
  const s = await serve({ post: [pending], get: [pending, pending, bound] });
  const c = clock();
  const { p, ids } = run(s, c);
  assert.deepEqual(await p, ALLOC);
  s.close();
  assert.deepEqual(s.seen.map((r) => `${r.method} ${r.url}`),
    ['POST /v1/allocations', ...Array(3).fill('GET /v1/allocations/abc123?wait=25s')]);
  assert.deepEqual(ids.got, ['abc123']);
});

test('a transient poll failure (503 upstream_error, 500, reset) polls the same id again, never POSTs again', async () => {
  const s = await serve({
    post: [pending],
    get: [err(503, 'upstream_error', { 'Retry-After': '2' }), { status: 500 }, { reset: true }, pending, bound],
  });
  const c = clock();
  const { p } = run(s, c);
  assert.deepEqual(await p, ALLOC);
  s.close();
  assert.equal(s.seen.filter((r) => r.method === 'POST').length, 1);
  assert.equal(s.seen.filter((r) => r.method === 'GET').length, 5);
  assert.deepEqual(c.waits.slice(0, 3), [2000, 2000, 2000]);
  assert.equal(c.waits.length, 4, 'and a pause after the instant 202');
});

test('pool_exhausted on a poll starts a fresh POST while the budget allows', async () => {
  const s = await serve({ post: [pending], get: [err(503, 'pool_exhausted', { 'Retry-After': '5' }), bound] });
  const c = clock();
  const { p, ids } = run(s, c);
  assert.deepEqual(await p, ALLOC);
  s.close();
  assert.deepEqual(s.seen.map((r) => r.method), ['POST', 'GET', 'POST', 'GET']);
  assert.deepEqual(ids.gone, ['abc123'], 'the exhausted id is dropped');
  assert.deepEqual(c.waits, [5000]);
});

test('pool_exhausted on a poll fails once the budget is spent', async () => {
  const s = await serve({ post: [pending], get: [err(503, 'pool_exhausted', { 'Retry-After': '5' })] });
  const c = clock();
  c.t = 0;
  const { p } = run(s, c, { budgetMs: 4000 });
  await assert.rejects(p, /allocate ci\/a failed: HTTP 503 pool_exhausted/);
  s.close();
  assert.equal(s.seen.filter((r) => r.method === 'POST').length, 1);
});

for (const [name, step, re] of [
  ['pod_lost', err(503, 'pod_lost'), /pod_lost/],
  ['not_found', err(404, 'not_found'), /not_found/],
  ['forbidden', err(403, 'forbidden'), /forbidden/],
]) {
  test(`${name} on a poll is terminal: no more polls, no new POST`, async () => {
    const s = await serve({ post: [pending], get: [step] });
    const c = clock();
    const { p, ids } = run(s, c);
    await assert.rejects(p, re);
    s.close();
    assert.deepEqual(s.seen.map((r) => r.method), ['POST', 'GET']);
    assert.deepEqual(ids.gone, ['abc123']);
  });
}

test('polling stops when the budget is spent and keeps the id known', async () => {
  const s = await serve({ post: [pending], get: [{ status: 500 }] });
  const c = clock();
  const { p, ids } = run(s, c, { budgetMs: 10000 });
  await assert.rejects(p, /abc123 was not bound within the queue budget/);
  s.close();
  assert.deepEqual(ids.got, ['abc123']);
  assert.deepEqual(ids.gone, [], 'still to be released');
});

test('a 202 with an id that is not what the controller mints is refused', async () => {
  const s = await serve({ post: [{ status: 202, body: JSON.stringify({ id: '../x', state: 'pending' }) }] });
  const { p, ids } = run(s, clock());
  await assert.rejects(p, /invalid allocation id/);
  s.close();
  assert.deepEqual(ids.got, []);
});

test('the POST itself still retries pool_exhausted 503 with Retry-After', async () => {
  const s = await serve({ post: [err(503, 'pool_exhausted', { 'Retry-After': '5' }), pending], get: [bound] });
  const c = clock();
  const { p } = run(s, c);
  assert.deepEqual(await p, ALLOC);
  s.close();
  assert.deepEqual(c.waits, [5000]);
});

test('a refused release answered 503 upstream_error with Retry-After is retried', async () => {
  let n = 0;
  const srv = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      if (++n === 1) { res.writeHead(503, { 'Retry-After': '2' }); return res.end(JSON.stringify({ error: { code: 'upstream_error', message: 'x' } })); }
      res.writeHead(204); res.end();
    });
  });
  await new Promise((r) => srv.listen(0, '127.0.0.1', r));
  const c = clock();
  await L.releaseAllocation({ api: `http://127.0.0.1:${srv.address().port}`, id: 'abc', getToken: async () => 't', sleep: c.sleep, now: c.now });
  srv.close();
  assert.equal(n, 2);
  assert.deepEqual(c.waits, [2000]);
});

const keysSent = (s) => s.seen.filter((r) => r.method === 'POST').map((r) => r.body.publicKey);

test('a POST cut after it was sent, or answered 5xx, is repeated with the same key, then followed', async () => {
  const s = await serve({ post: [{ reset: true }, { status: 500 }, err(503, 'upstream_error', { 'Retry-After': '2' }), pending], get: [bound] });
  const c = clock();
  const { p, ids } = run(s, c);
  assert.deepEqual(await p, ALLOC);
  s.close();
  const keys = keysSent(s);
  assert.equal(keys.length, 4);
  assert.ok(keys[0] && keys.every((k) => k === keys[0]), 'one key for every try');
  assert.deepEqual(c.waits, [1000, 2000, 2000]);
  assert.deepEqual(ids.got, ['abc123']);
});

test('a queue timeout, on a poll or on the POST, starts the next allocation with a new key', async () => {
  const s = await serve({
    post: [pending, err(503, 'pool_exhausted', { 'Retry-After': '1' }), { status: 201, body: bound.body }],
    get: [err(503, 'pool_exhausted', { 'Retry-After': '1' })],
  });
  const c = clock();
  const { p } = run(s, c);
  assert.deepEqual(await p, ALLOC);
  s.close();
  const keys = keysSent(s);
  assert.equal(new Set(keys).size, 3, 'each ended allocation gets a new key');
});

test('pod_lost on the POST ends the run without another POST', async () => {
  const s = await serve({ post: [err(503, 'pod_lost', { 'Retry-After': '1' })] });
  const { p } = run(s, clock());
  await assert.rejects(p, /pod_lost/);
  s.close();
  assert.equal(keysSent(s).length, 1);
});

test('a 202 poll answered at once is followed by a 1-2 s pause', async () => {
  const s = await serve({ post: [pending], get: [pending, pending, bound] });
  const c = clock();
  const { p } = run(s, c);
  await p;
  s.close();
  assert.equal(c.waits.length, 2);
  for (const w of c.waits) assert.ok(w >= 1000 && w < 2000, `pause ${w}`);
});

test('no fresh POST is sent once the queue timeout it would carry is 0', async () => {
  const s = await serve({ post: [err(503, 'pool_exhausted', { 'Retry-After': '30' })] });
  const c = clock();
  const { p } = run(s, c, {
    budgetMs: L.retryBudgetMs(100), queueSeconds: 100,
    attemptOpts: ({ remainingMs, publicKey }) => ({ method: 'POST', body: { publicKey, queueTimeoutSeconds: L.queueTimeoutFor(100, remainingMs) }, timeoutMs: 5000 }),
  });
  await assert.rejects(p, /pool_exhausted/);
  s.close();
  const posts = s.seen.filter((r) => r.method === 'POST').map((r) => r.body.queueTimeoutSeconds);
  assert.deepEqual(posts, [100, 70, 40, 10]);
});

test('a 401 on a poll keeps the id for the release', async () => {
  const s = await serve({ post: [pending], get: [err(401, 'unauthorized')] });
  const c = clock();
  const { p, ids } = run(s, c);
  await assert.rejects(p, /401/);
  s.close();
  assert.deepEqual(ids.gone, []);
});
