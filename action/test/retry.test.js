'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const net = require('node:net');
const L = require('../dist/lib');

/** What the controller sends with a 503 and Retry-After before it claims a worker. */
const BUSY = JSON.stringify({ error: { code: 'pool_exhausted', message: 'no free worker' } });

/** Local server that answers from `script` (one entry per request) and records the requests. */
async function serve(script) {
  const seen = [];
  const srv = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const raw = Buffer.concat(chunks).toString();
      seen.push({ method: req.method, auth: req.headers.authorization, body: raw ? JSON.parse(raw) : undefined });
      const step = script[Math.min(seen.length - 1, script.length - 1)];
      if (step.onRequest) step.onRequest();
      if (step.reset) return req.socket.destroy();
      res.writeHead(step.status, step.headers || {});
      res.end(step.body ?? (step.status === 503 && step.headers ? BUSY : ''));
    });
  });
  await new Promise((r) => srv.listen(0, '127.0.0.1', r));
  return { url: `http://127.0.0.1:${srv.address().port}/v1/allocations`, seen, close: () => srv.close() };
}

function harness(extra = {}) {
  const waits = [];
  const logs = [];
  let tokens = 0;
  return {
    waits, logs,
    opts: {
      what: 'allocate ci/amd64', budgetMs: L.retryBudgetMs(300), log: (m) => logs.push(m),
      sleep: async (ms) => { waits.push(ms); }, getToken: async () => `tok${++tokens}`, ...extra,
    },
  };
}

const post = { method: 'POST', body: { a: 1 }, headers: { 'Content-Type': 'application/json' } };

test('retries 503 with Retry-After, then succeeds', async () => {
  const s = await serve([{ status: 503, headers: { 'Retry-After': '5' } }, { status: 503, headers: { 'Retry-After': '2' } }, { status: 201, body: '{}' }]);
  const h = harness();
  const res = await L.requestWithRetry(s.url, post, h.opts);
  s.close();
  assert.equal(res.status, 201);
  assert.equal(s.seen.length, 3);
  assert.deepEqual(h.waits, [5000, 2000]);
  assert.equal(h.logs.length, 2);
  assert.match(h.logs[0], /HTTP 503.*5s/);
});

test('fetches a fresh token per attempt', async () => {
  const s = await serve([{ status: 503, headers: { 'Retry-After': '1' } }, { status: 201 }]);
  const h = harness();
  await L.requestWithRetry(s.url, post, h.opts);
  s.close();
  assert.deepEqual(s.seen.map((r) => r.auth), ['Bearer tok1', 'Bearer tok2']);
});

test('Retry-After is floored at 1s and capped at 30s', () => {
  assert.equal(L.retryAfterMs({ 'retry-after': '0' }), 1000);
  assert.equal(L.retryAfterMs({ 'retry-after': '600' }), 30000);
  assert.equal(L.retryAfterMs({ 'retry-after': '7' }), 7000);
  assert.equal(L.retryAfterMs({ 'retry-after': 'soon' }), undefined);
  assert.equal(L.retryAfterMs({}), undefined);
  const now = Date.parse('2030-01-01T00:00:00Z');
  assert.equal(L.retryAfterMs({ 'retry-after': new Date(now + 10000).toUTCString() }, now), 10000);
});

for (const [name, step] of [
  ['503 without Retry-After', { status: 503 }],
  ['503 with unusable Retry-After', { status: 503, headers: { 'Retry-After': 'later' } }],
  ['400', { status: 400, headers: { 'Retry-After': '1' } }],
  ['401', { status: 401 }],
  ['429 with Retry-After', { status: 429, headers: { 'Retry-After': '1' } }],
  ['500', { status: 500 }],
  ['504 with Retry-After', { status: 504, headers: { 'Retry-After': '1' } }],
]) {
  test(`no retry on ${name}`, async () => {
    const s = await serve([step, { status: 201 }]);
    const h = harness();
    const res = await L.requestWithRetry(s.url, post, h.opts);
    s.close();
    assert.equal(res.status, step.status);
    assert.equal(s.seen.length, 1);
    assert.deepEqual(h.waits, []);
  });
}

test('no retry on a reset after the request was sent', async () => {
  const s = await serve([{ reset: true }, { status: 201 }]);
  const h = harness();
  await assert.rejects(L.requestWithRetry(s.url, post, h.opts), (e) => e.code === 'ECONNRESET');
  s.close();
  assert.equal(s.seen.length, 1);
  assert.deepEqual(h.waits, []);
});

test('no retry on a timeout waiting for the response', async () => {
  const srv = http.createServer(() => {});
  await new Promise((r) => srv.listen(0, '127.0.0.1', r));
  const h = harness();
  const url = `http://127.0.0.1:${srv.address().port}/v1/allocations`;
  await assert.rejects(L.requestWithRetry(url, { ...post, timeoutMs: 100 }, h.opts), /timed out/);
  srv.closeAllConnections();
  srv.close();
  assert.deepEqual(h.waits, []);
});

test('retries connection refused, then succeeds', async () => {
  // Reserve a port, close it so the first attempt is refused, then listen on it.
  const probe = net.createServer();
  await new Promise((r) => probe.listen(0, '127.0.0.1', r));
  const port = probe.address().port;
  await new Promise((r) => probe.close(r));
  const srv = http.createServer((req, res) => { req.resume(); res.writeHead(201); res.end(); });
  const h = harness({
    sleep: async (ms) => { h.waits.push(ms); await new Promise((r) => srv.listen(port, '127.0.0.1', r)); },
  });
  const res = await L.requestWithRetry(`http://127.0.0.1:${port}/v1/allocations`, post, h.opts);
  srv.close();
  assert.equal(res.status, 201);
  assert.deepEqual(h.waits, [1000]);
  assert.match(h.logs[0], /ECONNREFUSED before send/);
});

test('isPreSendError accepts connect failures only', () => {
  assert.ok(L.isPreSendError({ code: 'ECONNREFUSED' }));
  assert.ok(L.isPreSendError({ code: 'ENOTFOUND' }));
  assert.ok(!L.isPreSendError({ code: 'ECONNRESET' }));
  assert.ok(!L.isPreSendError({ code: 'ETIMEDOUT' }));
  assert.ok(!L.isPreSendError(new Error('timed out')));
});

test('total time is bounded by queue timeout + 60s', async () => {
  assert.equal(L.retryBudgetMs(300), 360000);
  assert.equal(L.retryBudgetMs(undefined), 3660000);
  const s = await serve([{ status: 503, headers: { 'Retry-After': '30' } }]);
  let t = 0;
  const h = harness({ budgetMs: L.retryBudgetMs(0), now: () => t, sleep: async (ms) => { h.waits.push(ms); t += ms; } });
  const res = await L.requestWithRetry(s.url, post, h.opts);
  s.close();
  // 60 s budget: waits at t=0 and t=30 fit, the one at t=60 would pass the bound.
  assert.equal(res.status, 503);
  assert.deepEqual(h.waits, [30000, 30000]);
  assert.equal(s.seen.length, 3);
});

test('the bound also stops connection-error retries', async () => {
  const probe = net.createServer();
  await new Promise((r) => probe.listen(0, '127.0.0.1', r));
  const port = probe.address().port;
  await new Promise((r) => probe.close(r));
  let t = 0;
  const h = harness({ budgetMs: 5000, now: () => t, sleep: async (ms) => { h.waits.push(ms); t += ms; } });
  await assert.rejects(L.requestWithRetry(`http://127.0.0.1:${port}/x`, post, h.opts), (e) => e.code === 'ECONNREFUSED');
  assert.deepEqual(h.waits, [1000, 2000]);
});

test('DELETE retries 503 with Retry-After the same way', async () => {
  const s = await serve([{ status: 503, headers: { 'Retry-After': '3' } }, { status: 204 }]);
  const h = harness();
  const res = await L.requestWithRetry(s.url, { method: 'DELETE' }, { ...h.opts, idempotent: true });
  s.close();
  assert.equal(res.status, 204);
  assert.deepEqual(s.seen.map((r) => r.method), ['DELETE', 'DELETE']);
});

test('a retry shrinks the queue timeout to what is left of the budget', async () => {
  // A busy pool answers 503 only after the whole queue timeout, as the server does for pool_exhausted.
  let t = 0;
  const qt = 300;
  const busy = { status: 503, headers: { 'Retry-After': '5' }, onRequest: () => { t += (L.queueTimeoutFor(qt, 360000 - t) || 0) * 1000; } };
  const s = await serve([busy]);
  const sent = [];
  const h = harness({ budgetMs: L.retryBudgetMs(qt), now: () => t, sleep: async (ms) => { h.waits.push(ms); t += ms; } });
  const opts = ({ remainingMs }) => {
    const q = L.queueTimeoutFor(qt, remainingMs);
    sent.push(q);
    return { ...post, body: { queueTimeoutSeconds: q }, timeoutMs: L.REQUEST_TIMEOUT_MS };
  };
  const res = await L.requestWithRetry(s.url, opts, h.opts);
  s.close();
  assert.equal(res.status, 503);
  assert.deepEqual(s.seen.map((r) => r.body.queueTimeoutSeconds), sent);
  assert.equal(sent[0], 300);
  assert.ok(sent[1] < 300, 'retry must not ask for the full queue timeout again');
  assert.ok(t <= 360000, `total ${t} ms stays inside queue timeout + 60 s`);
});

test('queueTimeoutFor', () => {
  assert.equal(L.queueTimeoutFor(300, 360000), 300);
  assert.equal(L.queueTimeoutFor(300, 100000), 40);
  assert.equal(L.queueTimeoutFor(300, 30000), 0);
  assert.equal(L.queueTimeoutFor(undefined, 100000), undefined);
});

test('effectiveQueueTimeout: the input wins, else the pool default, else unknown', () => {
  const pool = { namespace: 'ci', name: 'amd64', queueTimeoutSeconds: 120 };
  assert.equal(L.effectiveQueueTimeout(300, pool), 300);
  assert.equal(L.effectiveQueueTimeout(0, pool), 0);
  assert.equal(L.effectiveQueueTimeout(undefined, pool), 120);
  assert.equal(L.effectiveQueueTimeout(undefined, { ...pool, queueTimeoutSeconds: 0 }), 0);
  // A server that predates the field, or a value that is not a whole number of seconds.
  assert.equal(L.effectiveQueueTimeout(undefined, { namespace: 'ci', name: 'amd64' }), undefined);
  assert.equal(L.effectiveQueueTimeout(undefined, { queueTimeoutSeconds: '120' }), undefined);
  assert.equal(L.effectiveQueueTimeout(undefined, undefined), undefined);
});

/**
 * Drive the allocate retry loop the way main.js does against a pool that stays busy: each attempt
 * holds the request for its whole queue timeout (the server's own default when the body has none),
 * then answers 503 pool_exhausted with Retry-After. Returns the queue timeouts sent and the total time.
 */
async function busyPool(inputQt, pool, serverDefault) {
  let t = 0;
  const sent = [];
  const pqt = L.effectiveQueueTimeout(inputQt, pool);
  const budgetMs = L.retryBudgetMs(pqt);
  const busy = {
    status: 503, headers: { 'Retry-After': '5' },
    onRequest: () => { const q = sent[sent.length - 1]; t += (q === undefined ? serverDefault : q) * 1000; },
  };
  const s = await serve([busy]);
  const h = harness({ budgetMs, now: () => t, sleep: async (ms) => { h.waits.push(ms); t += ms; } });
  const opts = ({ remainingMs }) => {
    const q = L.queueTimeoutFor(pqt, remainingMs);
    sent.push(q);
    return { ...post, body: L.allocationBody({ namespace: 'ci', pool: 'amd64' }, 'pk', undefined, q), timeoutMs: L.REQUEST_TIMEOUT_MS };
  };
  const res = await L.requestWithRetry(s.url, opts, h.opts);
  s.close();
  assert.equal(res.status, 503);
  assert.deepEqual(s.seen.map((r) => r.body.queueTimeoutSeconds), sent);
  return { sent, t };
}

test('without queue-timeout, the pool default bounds the retries', async () => {
  const { sent, t } = await busyPool(undefined, { queueTimeoutSeconds: 120 }, 120);
  assert.equal(sent[0], 120, 'the first attempt queues for the pool default');
  assert.ok(sent.length > 1 && sent.slice(1).every((q) => q < 120), `${sent}`);
  assert.ok(t <= 180000, `total ${t} ms stays inside the pool default + 60 s`);
});

test('an explicit queue-timeout still wins over the pool default', async () => {
  const { sent, t } = await busyPool(30, { queueTimeoutSeconds: 120 }, 120);
  assert.equal(sent[0], 30);
  assert.ok(t <= 90000, `total ${t} ms stays inside queue-timeout + 60 s`);
});

test('a server that reports no pool default keeps the 1 h budget', async () => {
  const { sent } = await busyPool(undefined, {}, 120);
  assert.ok(sent.every((q) => q === undefined), 'the server default is not overridden');
  assert.equal(L.retryBudgetMs(L.effectiveQueueTimeout(undefined, {})), 3660000);
});

for (const [name, body] of [
  ['an empty body', ''],
  ['an error code that may follow a claim', JSON.stringify({ error: { code: 'ca_unavailable', message: 'x' } })],
  ['a body that is not JSON', '<html>overloaded</html>'],
]) {
  test(`POST is not retried on 503 with Retry-After and ${name}`, async () => {
    const s = await serve([{ status: 503, headers: { 'Retry-After': '1' }, body }, { status: 201 }]);
    const h = harness();
    const res = await L.requestWithRetry(s.url, post, h.opts);
    s.close();
    assert.equal(res.status, 503);
    assert.equal(s.seen.length, 1);
  });
}

for (const code of ['pool_exhausted', 'start_timeout', 'shutting_down', 'upstream_error']) {
  test(`POST is retried on 503 with Retry-After and code ${code}`, async () => {
    const body = JSON.stringify({ error: { code, message: 'x' } });
    const s = await serve([{ status: 503, headers: { 'Retry-After': '1' }, body }, { status: 201 }]);
    const h = harness();
    const res = await L.requestWithRetry(s.url, post, h.opts);
    s.close();
    assert.equal(res.status, 201);
  });
}

test('an idempotent request retries any 503 with Retry-After', async () => {
  const s = await serve([{ status: 503, headers: { 'Retry-After': '1' }, body: '' }, { status: 200, body: '{}' }]);
  const h = harness();
  const res = await L.requestWithRetry(s.url, { method: 'GET' }, { ...h.opts, idempotent: true });
  s.close();
  assert.equal(res.status, 200);
});

test('a release gives up after 60 s, whatever the queue timeout', async () => {
  const s = await serve([{ status: 503, headers: { 'Retry-After': '30' }, body: '' }]);
  let t = 0;
  const waits = [];
  const api = s.url.replace('/v1/allocations', '');
  await assert.rejects(
    L.releaseAllocation({ api, id: 'abc', getToken: async () => 't', log: () => {}, now: () => t, sleep: async (ms) => { waits.push(ms); t += ms; } }),
    /release abc failed: HTTP 503/,
  );
  s.close();
  assert.equal(L.SHORT_BUDGET_MS, 60000);
  assert.deepEqual(waits, [30000, 30000]);
});

test('a release stops retrying an unreachable API after 60 s', async () => {
  const probe = net.createServer();
  await new Promise((r) => probe.listen(0, '127.0.0.1', r));
  const port = probe.address().port;
  await new Promise((r) => probe.close(r));
  let t = 0;
  const waits = [];
  await assert.rejects(
    L.releaseAllocation({ api: `http://127.0.0.1:${port}`, id: 'abc', getToken: async () => 't', now: () => t, sleep: async (ms) => { waits.push(ms); t += ms; } }),
    (e) => e.code === 'ECONNREFUSED',
  );
  assert.ok(t <= 60000, `waited ${t} ms`);
});

test('a release counts 404 as released', async () => {
  const s = await serve([{ status: 404, body: '{}' }]);
  await L.releaseAllocation({ api: s.url.replace('/v1/allocations', ''), id: 'abc', getToken: async () => 't' });
  s.close();
});
