'use strict';
// dist/main.js and dist/post.js choose the tunnel or tcp from the endpoint the controller reports.
const test = require('node:test');
const assert = require('node:assert/strict');
const net = require('node:net');
const path = require('node:path');
const { fixture, run, readState, deletes, sleep, skipNoOpenssl: skip } = require('./helpers');

const DIST = path.join(__dirname, '..', 'dist');
const main = (env) => run(process.execPath, [path.join(DIST, 'main.js')], env);
const post = (env, state) => run(process.execPath, [path.join(DIST, 'post.js')],
  { ...env, ...Object.fromEntries(Object.entries(state).map(([k, v]) => [`STATE_${k}`, v])) });
const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };
const creates = (f) => f.docker().filter((l) => l.startsWith('buildx create'));

test('a path endpoint starts one forwarder and buildx gets a local port per pool', { skip }, async (t) => {
  const f = await fixture(t, { endpoint: '/v1/tunnel' });
  t.after(() => { const p = Number(readState(f.env.GITHUB_STATE).forwarder); if (p) try { process.kill(p); } catch { /* gone */ } });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a\nci/b' });
  assert.equal(m.code, 0, m.out);
  const state = readState(f.env.GITHUB_STATE);
  const pid = Number(state.forwarder);
  assert.ok(pid > 1);
  assert.ok(alive(pid), 'the forwarder outlives the main step');
  const [first, second] = creates(f);
  const ports = [first, second].map((l) => /tcp:\/\/127\.0\.0\.1:(\d+)$/.exec(l)[1]);
  assert.notEqual(ports[0], ports[1]);
  assert.match(first, /--driver-opt cert=\S+,key=\S+,servername=e\.example tcp:\/\/127\.0\.0\.1:\d+$/);
  assert.match(second, /--append --node n1/);
  assert.ok(!f.docker().join('\n').includes('/v1/tunnel'), 'buildx never sees the path');

  // A connection to the local port reaches the controller at /v1/tunnel over TLS that trusts ca-cert.
  const s = net.connect({ port: Number(ports[0]), host: '127.0.0.1' });
  s.on('error', () => {});
  const end = Date.now() + 5000;
  while (!f.calls.some((c) => c.url === '/v1/tunnel') && Date.now() < end) await sleep(20);
  s.destroy();
  const hit = f.calls.find((c) => c.url === '/v1/tunnel');
  assert.ok(hit, 'the forwarder dialled the API base URL');
  assert.equal(hit.auth, undefined, 'the tunnel request carries no bearer token');

  const p = await post(f.env, state);
  assert.equal(p.code, 0, p.out);
  const stop = Date.now() + 5000;
  while (alive(pid) && Date.now() < stop) await sleep(20);
  assert.ok(!alive(pid), 'post stops the forwarder');
  assert.ok(f.docker().includes('buildx rm --force bk-id1a'));
  assert.deepEqual(deletes(f.calls).sort(), ['/v1/allocations/id1a', '/v1/allocations/id2b']);
  assert.ok(!p.out.includes('::warning::'), p.out);
  assert.match(p.out, /::group::buildkit tunnel forwarder errors\n.* https:\/\/127\.0\.0\.1:\d+\/v1\/tunnel tunnel refused: HTTP 404\n::endgroup::/, 'the refusal shows in a collapsed group');
  const group = p.out.split('::group::')[1].split('::endgroup::')[0];
  assert.ok(!/oidc-token|CERT|bearer/.test(group), 'the group holds no token or certificate');
});

test('a tcp:// endpoint goes to buildx unchanged and no forwarder starts', { skip }, async (t) => {
  const f = await fixture(t);
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a\nci/b' });
  assert.equal(m.code, 0, m.out);
  const state = readState(f.env.GITHUB_STATE);
  assert.equal(state.forwarder, undefined);
  for (const l of creates(f)) assert.ok(l.endsWith(' tcp://e.example:8444'), l);
  const p = await post(f.env, state);
  assert.equal(p.code, 0, p.out);
  assert.ok(!p.out.includes('::warning::'), p.out);
});

test('post is quiet when the forwarder is already gone', { skip }, async (t) => {
  const f = await fixture(t, { endpoint: '/v1/tunnel' });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 0, m.out);
  const state = readState(f.env.GITHUB_STATE);
  process.kill(Number(state.forwarder));
  const end = Date.now() + 5000;
  while (alive(Number(state.forwarder)) && Date.now() < end) await sleep(20);
  const p = await post(f.env, state);
  assert.equal(p.code, 0, p.out);
  assert.ok(!p.out.includes('::warning::'), p.out);
  assert.deepEqual(deletes(f.calls), ['/v1/allocations/id1a']);
  assert.ok(!p.out.includes('::group::'), 'an empty forwarder log prints nothing');
});

test('a failed buildx create still leaves the forwarder pid for post to stop', { skip }, async (t) => {
  const f = await fixture(t, { endpoint: '/v1/tunnel', dockerFails: true });
  const m = await main({ ...f.env, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 1);
  const state = readState(f.env.GITHUB_STATE);
  const pid = Number(state.forwarder);
  assert.ok(alive(pid));
  const p = await post(f.env, state);
  assert.equal(p.code, 0, p.out);
  const end = Date.now() + 5000;
  while (alive(pid) && Date.now() < end) await sleep(20);
  assert.ok(!alive(pid));
});

test('an empty ca-cert falls back to the file in NODE_EXTRA_CA_CERTS, and a missing file fails naming it', { skip }, async (t) => {
  const f = await fixture(t);
  const m = await main({ ...f.env, 'INPUT_CA-CERT': '', NODE_EXTRA_CA_CERTS: f.caFile, INPUT_POOLS: 'ci/a' });
  assert.equal(m.code, 0, m.out);
  assert.equal(readState(f.env.GITHUB_STATE).ca, `# with built-in roots\n${require('node:fs').readFileSync(f.caFile, 'utf8')}`);
  const bad = await main({ ...f.env, 'INPUT_CA-CERT': '', SSL_CERT_FILE: '/nonexistent/ca.pem', INPUT_POOLS: 'ci/a' });
  assert.equal(bad.code, 1);
  assert.match(bad.out, /SSL_CERT_FILE names \/nonexistent\/ca\.pem/);
});
