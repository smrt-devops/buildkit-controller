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
