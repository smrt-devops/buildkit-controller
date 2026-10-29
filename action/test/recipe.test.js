'use strict';
// Runs docs/ci/buildkit-allocate.sh against the local fixture.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
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

test('a POST cut after it was sent, or answered 5xx, is repeated with the same key', { skip }, async (t) => {
  let posts = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (rec.method !== 'POST') return undefined;
      posts += 1;
      if (posts === 1) return { reset: true };
      if (posts === 2) return { status: 502, body: '<html>bad gateway</html>' };
      if (posts === 3) return { status: 503, headers: { 'Retry-After': '1' }, body: JSON.stringify({ error: { code: 'upstream_error', message: 'x' } }) };
      return undefined;
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 0, r.out);
  const keys = f.calls.filter((c) => c.method === 'POST').map((c) => c.body.publicKey);
  assert.equal(keys.length, 4);
  assert.ok(keys[0] && keys.every((k) => k === keys[0]), 'one key for every try');
  assert.match(r.out, /retrying with the same key/);
});

test('a pool_exhausted 503 on the POST allocates again with a new key', { skip }, async (t) => {
  let posts = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (rec.method !== 'POST' || ++posts > 1) return undefined;
      return { status: 503, headers: { 'Retry-After': '1' }, body: JSON.stringify({ error: { code: 'pool_exhausted', message: 'x' } }) };
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 0, r.out);
  const keys = f.calls.filter((c) => c.method === 'POST').map((c) => c.body.publicKey);
  assert.equal(new Set(keys).size, 2);
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

const queued = (id) => ({ status: 202, body: JSON.stringify({ id, state: 'pending', position: 1 }) });
const boundBody = (id) => ({
  status: 200, body: JSON.stringify({ id, namespace: 'ci', pool: 'a', clientCert: 'CERT', endpoint: 'tcp://e.example:8444', serverName: 'e.example' }),
});
const errBody = (status, code, headers) => ({ status, headers, body: JSON.stringify({ error: { code, message: 'x' } }) });
const isPost = (rec) => rec.method === 'POST' && rec.url === '/v1/allocations';
const isPoll = (rec) => rec.method === 'GET' && rec.url.startsWith('/v1/allocations/');

test('a 202 is polled until bound, then the command runs and the worker is released', { skip }, async (t) => {
  let polls = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) return ++polls < 3 ? queued('q1') : boundBody('q1');
      return undefined;
    },
  });
  const r = await sh(f, ['sh', '-c', 'test "$BUILDER" = bk-q1']);
  assert.equal(r.code, 0, r.out);
  assert.equal(f.calls.filter(isPost).length, 1);
  assert.deepEqual(f.calls.filter(isPoll).map((c) => c.url), Array(3).fill('/v1/allocations/q1?wait=25s'));
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/q1']);
});

test('a transient poll failure polls the same id again', { skip }, async (t) => {
  let polls = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) {
        polls += 1;
        if (polls === 1) return errBody(503, 'upstream_error', { 'Retry-After': '1' });
        if (polls === 2) return { status: 500, body: '' };
        return boundBody('q1');
      }
      return undefined;
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 0, r.out);
  assert.equal(f.calls.filter(isPost).length, 1);
  assert.equal(polls, 3);
});

test('pool_exhausted on a poll allocates again within the budget', { skip }, async (t) => {
  let posts = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued(`q${++posts}`);
      if (isPoll(rec)) return rec.url.includes('/q1?') ? errBody(503, 'pool_exhausted', { 'Retry-After': '1' }) : boundBody('q2');
      return undefined;
    },
  });
  const r = await sh(f, ['sh', '-c', 'test "$BUILDER" = bk-q2'], { QUEUE_TIMEOUT: '300' });
  assert.equal(r.code, 0, r.out);
  assert.equal(posts, 2);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/q2'], 'the exhausted id is not released');
  assert.equal(new Set(f.calls.filter(isPost).map((c) => c.body.publicKey)).size, 2, 'the next allocation has a new key');
});

test('pod_lost on a poll fails the run without another POST', { skip }, async (t) => {
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) return errBody(503, 'pod_lost');
      return undefined;
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 1);
  assert.match(r.out, /allocation failed: HTTP 503 pod_lost/);
  assert.equal(f.calls.filter(isPost).length, 1);
  assert.deepEqual(deletes(f.calls), []);
});

test('a TERM while the allocation is queued releases its id', { skip }, async (t) => {
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) return errBody(503, 'upstream_error', { 'Retry-After': '30' });
      return undefined;
    },
  });
  const p = spawn('sh', [SCRIPT, 'true'], { env: env(f) });
  const closed = new Promise((r) => p.on('close', (code) => r(code)));
  for (let i = 0; i < 100 && !f.calls.some(isPoll); i++) await sleep(100);
  await sleep(300);
  p.kill('SIGTERM');
  assert.equal(await closed, 143);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/q1']);
});

test('a release answered 503 upstream_error with Retry-After is retried', { skip }, async (t) => {
  let n = 0;
  const f = await fixture(t, {
    override: (rec) => (rec.method === 'DELETE' && ++n === 1 ? errBody(503, 'upstream_error', { 'Retry-After': '1' }) : undefined),
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 0, r.out);
  assert.equal(deletes(f.calls).length, 2);
  assert.ok(!r.out.includes('warning: release'), r.out);
});

const poolsWith = (qt) => ({ status: 200, body: JSON.stringify({ pools: [{ namespace: 'ci', name: 'a', ready: true, ...(qt === undefined ? {} : { queueTimeoutSeconds: qt }) }] }) });
const isPools = (rec) => rec.method === 'GET' && rec.url === '/v1/pools';

test('the command does not inherit TOKEN', { skip }, async (t) => {
  const f = await fixture(t);
  const r = await sh(f, ['sh', '-c', 'test -z "${TOKEN+set}"']);
  assert.equal(r.code, 0, r.out);
});

test('TOKEN_FILE is read again for every request, the release included', { skip }, async (t) => {
  const f = await fixture(t);
  const tf = path.join(f.dir, 'token');
  fs.writeFileSync(tf, 'old\n');
  const e = env(f, { TOKEN_FILE: tf });
  delete e.TOKEN;
  const r = await run('sh', [SCRIPT, 'sh', '-c', `printf new > '${tf}'`], e);
  assert.equal(r.code, 0, r.out);
  assert.equal(f.calls.find(isPost).auth, 'Bearer old');
  assert.equal(f.calls.find((c) => c.method === 'DELETE').auth, 'Bearer new');
});

test('a 401 on a poll still releases the id', { skip }, async (t) => {
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) return errBody(401, 'unauthorized');
      return undefined;
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 1);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/q1']);
});

test('a 202 poll answered at once is followed by a pause of at least 1 s', { skip }, async (t) => {
  let polls = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) return ++polls < 2 ? queued('q1') : boundBody('q1');
      return undefined;
    },
  });
  const r = await sh(f, ['true']);
  assert.equal(r.code, 0, r.out);
  const [a, b] = f.calls.filter(isPoll);
  assert.ok(b.at - a.at >= 900, `gap ${b.at - a.at} ms`);
});

/** A curl on PATH that records its --max-time values, then runs the real curl. */
function curlSpy(f) {
  const real = spawnSync('which', ['curl'], { encoding: 'utf8' }).stdout.trim();
  const bin = path.join(f.dir, 'spy');
  fs.mkdirSync(bin);
  const log = path.join(f.dir, 'curl.log');
  fs.writeFileSync(path.join(bin, 'curl'),
    `#!/bin/sh\nfor a in "$@"; do case $a in */v1/allocations) post=1;; esac; done\nprev=\nfor a in "$@"; do [ "$prev" != --max-time ] || [ -z "\${post:-}" ] || echo "$a" >> '${log}'; prev=$a; done\nexec '${real}' "$@"\n`, { mode: 0o755 });
  f.env.PATH = `${bin}:${f.env.PATH}`;
  return () => (fs.existsSync(log) ? fs.readFileSync(log, 'utf8').trim().split('\n').map(Number) : []);
}

test('a POST has a 40 s limit on the current server and the whole budget on v0.1.0', { skip }, async (t) => {
  for (const [qt, want] of [[300, (m) => m <= 40], [undefined, (m) => m > 3000]]) {
    const f = await fixture(t, { override: (rec) => (isPools(rec) ? poolsWith(qt) : undefined) });
    const limits = curlSpy(f);
    const r = await sh(f, ['true']);
    assert.equal(r.code, 0, r.out);
    const m = limits();
    assert.equal(m.length, 1, String(m));
    assert.ok(want(m[0]), `queueTimeoutSeconds ${qt}: --max-time ${m[0]}`);
  }
});

test('no fresh POST is sent once the queue timeout it would carry is 0', { skip, timeout: 30000 }, async (t) => {
  const f = await fixture(t, {
    override: (rec) => (isPost(rec) ? errBody(503, 'pool_exhausted', { 'Retry-After': '1' }) : undefined),
  });
  const r = await sh(f, ['true'], { QUEUE_TIMEOUT: '2' });
  assert.equal(r.code, 1);
  assert.ok(f.calls.filter(isPost).length <= 3, `${f.calls.filter(isPost).length} POSTs`);
});
