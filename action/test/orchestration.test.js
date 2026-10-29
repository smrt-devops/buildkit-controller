'use strict';
// Runs dist/main.js and dist/post.js as the runner does, against the local fixture.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { fixture, run, readState, deletes, skipNoOpenssl: skip } = require('./helpers');

const DIST = path.join(__dirname, '..', 'dist');
const main = (env) => run(process.execPath, [path.join(DIST, 'main.js')], env);
const post = (env, state) => run(process.execPath, [path.join(DIST, 'post.js')],
  { ...env, ...Object.fromEntries(Object.entries(state).map(([k, v]) => [`STATE_${k}`, v])) });

test('a successful run saves its state, and the post step releases and cleans up', { skip }, async (t) => {
  const f = await fixture(t);
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 0, m.out);
  const state = readState(f.env.GITHUB_STATE);
  assert.equal(state.ids, 'id1a');
  assert.equal(state.builder, 'bk-id1a');
  assert.equal(state.api, f.env.INPUT_URL);
  assert.ok(fs.existsSync(state.dirs), 'key material dir exists');
  assert.ok(f.docker().some((l) => l.startsWith('buildx create --name bk-id1a')));

  const p = await post(f.env, state);
  assert.equal(p.code, 0, p.out);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/id1a']);
  assert.ok(f.docker().includes('buildx rm --force bk-id1a'));
  assert.ok(!fs.existsSync(state.dirs), 'key material dir removed');
  assert.ok(!p.out.includes('not released'));
});

test('state is saved before the first POST, so a failed allocation leaves nothing to release', { skip }, async (t) => {
  const f = await fixture(t, { deny: ['a'] });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 1);
  const state = readState(f.env.GITHUB_STATE);
  assert.equal(state.api, f.env.INPUT_URL);
  assert.equal(state.ids, undefined);
  const p = await post(f.env, state);
  assert.equal(p.code, 0, p.out);
  assert.deepEqual(deletes(f.calls), []);
});

test('when one of two pools fails, the pool that succeeded is released and its id kept in state', { skip }, async (t) => {
  const f = await fixture(t, { deny: ['b'] });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a\nci/b' });
  assert.equal(m.code, 1);
  assert.match(m.out, /allocate ci\/b failed: HTTP 403/);
  const state = readState(f.env.GITHUB_STATE);
  assert.equal(state.ids, 'id1a');
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/id1a'], 'main released the successful pool');
  assert.equal(state.builder, undefined);
});

test('a release answered 404 counts as released', { skip }, async (t) => {
  const f = await fixture(t, { deleteStatus: 404 });
  await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  const p = await post(f.env, readState(f.env.GITHUB_STATE));
  assert.equal(p.code, 0);
  assert.ok(!p.out.includes('not released'), p.out);
});

test('a release the controller refuses is a warning, not a failure of the step', { skip }, async (t) => {
  const f = await fixture(t, { deleteStatus: 403 });
  await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  const p = await post(f.env, readState(f.env.GITHUB_STATE));
  assert.equal(p.code, 0);
  assert.match(p.out, /::warning::release id1a failed: HTTP 403/);
  assert.match(p.out, /some allocations were not released/);
});

test('the pool listing rides through a 503 with Retry-After', { skip }, async (t) => {
  let first = true;
  const f = await fixture(t, {
    override: (rec) => {
      if (rec.method === 'GET' && first) { first = false; return { status: 503, headers: { 'Retry-After': '1' }, body: '{}' }; }
      return undefined;
    },
  });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 0, m.out);
  assert.equal(f.calls.filter((c) => c.method === 'GET').length, 2);
});

test('builder state is written only once buildx create succeeded', { skip }, async (t) => {
  const f = await fixture(t, { dockerFails: true });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 1);
  const state = readState(f.env.GITHUB_STATE);
  assert.equal(state.builder, undefined);
  assert.equal(state.ids, 'id1a');
  const p = await post(f.env, state);
  assert.equal(p.code, 0);
  assert.ok(!f.docker().some((l) => l.startsWith('buildx rm')), 'no builder removed that this run did not create');
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/id1a']);
  assert.ok(!fs.existsSync(state.dirs), 'key material dir removed');
});

const queued = (id) => ({ status: 202, body: JSON.stringify({ id, state: 'pending', position: 1 }) });
const boundBody = (id) => ({
  status: 200, body: JSON.stringify({ id, namespace: 'ci', pool: 'a', clientCert: 'CERT', endpoint: 'tcp://e.example:8444', serverName: 'e.example' }),
});
const isPost = (rec) => rec.method === 'POST' && rec.url === '/v1/allocations';
const isPoll = (rec) => rec.method === 'GET' && rec.url.startsWith('/v1/allocations/');

test('a queued allocation is polled until bound, then used and released like any other', { skip }, async (t) => {
  let polls = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) return ++polls < 3 ? { status: 202, body: JSON.stringify({ id: 'q1', state: 'pending' }) } : boundBody('q1');
      return undefined;
    },
  });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 0, m.out);
  assert.equal(f.calls.filter(isPost).length, 1);
  assert.deepEqual(f.calls.filter(isPoll).map((c) => c.url), Array(3).fill('/v1/allocations/q1?wait=25s'));
  const state = readState(f.env.GITHUB_STATE);
  assert.equal(state.ids, 'q1');
  assert.equal(state.builder, 'bk-q1');
  const p = await post(f.env, state);
  assert.equal(p.code, 0, p.out);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/q1']);
});

test('a transient poll failure polls the same id again', { skip }, async (t) => {
  let polls = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) return ++polls === 1 ? { status: 503, headers: { 'Retry-After': '1' }, body: JSON.stringify({ error: { code: 'upstream_error', message: 'x' } }) } : boundBody('q1');
      return undefined;
    },
  });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 0, m.out);
  assert.equal(f.calls.filter(isPost).length, 1);
  assert.equal(f.calls.filter(isPoll).length, 2);
});

test('pool_exhausted on a poll allocates again within the budget', { skip }, async (t) => {
  let posts = 0;
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued(`q${++posts}`);
      if (isPoll(rec)) {
        return rec.url.includes('/q1?')
          ? { status: 503, headers: { 'Retry-After': '1' }, body: JSON.stringify({ error: { code: 'pool_exhausted', message: 'x' } }) }
          : boundBody('q2');
      }
      return undefined;
    },
  });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a', 'INPUT_QUEUE-TIMEOUT': '300' });
  assert.equal(m.code, 0, m.out);
  assert.equal(posts, 2);
  const sent = f.calls.filter(isPost).map((c) => c.body.queueTimeoutSeconds);
  assert.ok(sent[0] >= 299 && sent[0] <= 300, `first POST queues for the input: ${sent[0]}`);
  assert.ok(sent[1] <= 300 && sent[1] > 0, `second POST queues for what is left: ${sent[1]}`);
  assert.equal(readState(f.env.GITHUB_STATE).ids, 'q2', 'the exhausted id is not kept');
});

test('a cancel while the allocation is queued leaves its id in state, and the post step releases it', { skip }, async (t) => {
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return queued('q1');
      if (isPoll(rec)) return { status: 500, body: '' };
      return undefined;
    },
  });
  const child = require('node:child_process').spawn(process.execPath, [path.join(DIST, 'main.js')], { env: { ...f.env, INPUT_POOLS: 'ci/a' } });
  for (let i = 0; i < 100 && !f.calls.some(isPoll); i++) await new Promise((r) => setTimeout(r, 50));
  assert.ok(f.calls.some(isPoll), 'polling started');
  const closed = new Promise((r) => child.on('close', r));
  child.kill('SIGKILL');
  await closed;
  const state = readState(f.env.GITHUB_STATE);
  assert.equal(state.ids, 'q1');
  const p = await post(f.env, state);
  assert.equal(p.code, 0, p.out);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/q1']);
});

test('a poll that fails for good after another pool bound releases the bound one, not the dead id', { skip }, async (t) => {
  const f = await fixture(t, {
    override: (rec) => {
      if (isPost(rec)) return rec.body.pool === 'b' ? queued('qb') : undefined;
      if (isPoll(rec)) return { status: 503, body: JSON.stringify({ error: { code: 'pod_lost', message: 'x' } }) };
      return undefined;
    },
  });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a\nci/b' });
  assert.equal(m.code, 1);
  assert.match(m.out, /pod_lost/);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/id1a']);
});

test('a release answered 503 upstream_error with Retry-After is retried', { skip }, async (t) => {
  let n = 0;
  const f = await fixture(t, {
    override: (rec) => (rec.method === 'DELETE' && ++n === 1
      ? { status: 503, headers: { 'Retry-After': '1' }, body: JSON.stringify({ error: { code: 'upstream_error', message: 'x' } }) } : undefined),
  });
  await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  const p = await post(f.env, readState(f.env.GITHUB_STATE));
  assert.equal(p.code, 0, p.out);
  assert.equal(deletes(f.calls).length, 2);
  assert.ok(!p.out.includes('not released'), p.out);
});
