'use strict';
// Runs docs/ci/buildkit-allocate.sh against the local fixture.
const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { fixture, run, deletes, sleep, skipNoOpenssl, have } = require('./helpers');

const SCRIPT = path.join(__dirname, '..', '..', 'docs', 'ci', 'buildkit-allocate.sh');
const skip = skipNoOpenssl || (have('curl') && have('jq') ? false : 'curl or jq not available');
const env = (f, extra = {}) => ({ ...f.env, API: f.env.INPUT_URL, API_CA: f.caFile, POOL: 'ci/a', TOKEN: 't', ...extra });
const sh = (f, args, extra) => run('sh', [SCRIPT, ...args], env(f, extra));

test('the command runs, then the worker is released', { skip }, async (t) => {
  const f = await fixture(t);
  const r = await sh(f, ['sh', '-c', 'test -n "$BUILDER"']);
  assert.equal(r.code, 0, r.out);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/id1a']);
});

test('the command keeps the original stdin', { skip }, async (t) => {
  const f = await fixture(t);
  const p = spawn('sh', [SCRIPT, 'sh', '-c', 'read x; test "$x" = hello'], { env: env(f) });
  p.stdin.end('hello\n');
  const code = await new Promise((r) => p.on('close', r));
  assert.equal(code, 0);
});

test('a TERM while the command runs releases at once, without waiting for the command', { skip }, async (t) => {
  const f = await fixture(t);
  const p = spawn('sh', [SCRIPT, 'sleep', '60'], { env: env(f) });
  const closed = new Promise((r) => p.on('close', (code, signal) => r({ code, signal })));
  for (let i = 0; i < 100 && !f.calls.some((c) => c.method === 'POST'); i++) await sleep(100);
  await sleep(1500);
  const sent = Date.now();
  p.kill('SIGTERM');
  const r = await closed;
  assert.equal(r.code, 143);
  assert.ok(Date.now() - sent < 10000, 'exited promptly');
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/id1a']);
});

test('QUEUE_TIMEOUT and TTL must be whole seconds', { skip }, async (t) => {
  const f = await fixture(t);
  for (const bad of [{ QUEUE_TIMEOUT: '5m' }, { TTL: '1h' }]) {
    const r = await sh(f, ['true'], bad);
    assert.equal(r.code, 2);
    assert.match(r.out, /must be whole seconds/);
  }
  assert.equal(f.calls.length, 0);
});

test('an allocation id that is not what the controller mints is refused', { skip }, async (t) => {
  const f = await fixture(t, {
    override: (rec) => (rec.method === 'POST'
      ? { status: 201, body: JSON.stringify({ id: '../x', clientCert: 'C', endpoint: 'tcp://e:1', serverName: 'e' }) } : undefined),
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 1);
  assert.match(r.out, /invalid allocation id/);
  assert.deepEqual(deletes(f.calls), []);
});

test('a 503 with Retry-After is retried only for a code the controller sends before it claims', { skip }, async (t) => {
  let posts = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (rec.method !== 'POST') return undefined;
      posts += 1;
      return { status: 503, headers: { 'Retry-After': '1' }, body: '' };
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 1);
  assert.equal(posts, 1, 'an intermediary 503 is not retried');
});

test('a pool_exhausted 503 with Retry-After is retried', { skip }, async (t) => {
  let posts = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (rec.method !== 'POST' || ++posts > 1) return undefined;
      return { status: 503, headers: { 'Retry-After': '1' }, body: JSON.stringify({ error: { code: 'pool_exhausted', message: 'x' } }) };
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 0, r.out);
  assert.equal(posts, 2);
});

test('the pool listing is retried on 503 with Retry-After', { skip }, async (t) => {
  let first = true;
  const f = await fixture(t, {
    override: (rec) => {
      if (rec.method === 'GET' && first) { first = false; return { status: 503, headers: { 'Retry-After': '1' }, body: '{}' }; }
      return undefined;
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 0, r.out);
  assert.equal(f.calls.filter((c) => c.method === 'GET').length, 2);
});
