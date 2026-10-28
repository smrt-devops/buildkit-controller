'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const net = require('node:net');
const L = require('../dist/lib');

/** Local server that answers from `script` (one entry per request) and records the requests. */
async function serve(script) {
  const seen = [];
  const srv = http.createServer((req, res) => {
    seen.push({ method: req.method, auth: req.headers.authorization });
    req.resume();
    const step = script[Math.min(seen.length - 1, script.length - 1)];
    if (step.reset) return req.socket.destroy();
    res.writeHead(step.status, step.headers || {});
    res.end(step.body || '');
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
  const res = await L.requestWithRetry(s.url, { method: 'DELETE' }, h.opts);
  s.close();
  assert.equal(res.status, 204);
  assert.deepEqual(s.seen.map((r) => r.method), ['DELETE', 'DELETE']);
});
