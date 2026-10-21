'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const L = require('../dist/lib');

test('parseList splits commas and newlines', () => {
  assert.deepEqual(L.parseList('ci/a, ci/b\n\n ci/c\n'), ['ci/a', 'ci/b', 'ci/c']);
  assert.deepEqual(L.parseList(''), []);
});

test('parsePool', () => {
  assert.deepEqual(L.parsePool('ci/amd64'), { namespace: 'ci', pool: 'amd64' });
  for (const bad of ['amd64', 'a/b/c', 'Ci/x', '/x', 'x/']) assert.throws(() => L.parsePool(bad));
});

test('parseDuration', () => {
  assert.equal(L.parseDuration('3600'), 3600);
  assert.equal(L.parseDuration('90s'), 90);
  assert.equal(L.parseDuration('5m'), 300);
  assert.equal(L.parseDuration('1h'), 3600);
  assert.equal(L.parseDuration(''), undefined);
  assert.throws(() => L.parseDuration('1d'));
});

test('generateKey makes a P-256 pair', () => {
  const k = L.generateKey();
  assert.match(k.privateKey, /BEGIN PRIVATE KEY/);
  assert.match(k.publicKey, /BEGIN PUBLIC KEY/);
  const pub = crypto.createPublicKey(k.publicKey);
  assert.equal(pub.asymmetricKeyDetails.namedCurve, 'prime256v1');
});

test('allocationBody omits unset fields', () => {
  assert.deepEqual(L.allocationBody({ namespace: 'ci', pool: 'a' }, 'PK'), { namespace: 'ci', pool: 'a', publicKey: 'PK' });
  assert.deepEqual(L.allocationBody({ namespace: 'ci', pool: 'a' }, 'PK', 60, 0),
    { namespace: 'ci', pool: 'a', publicKey: 'PK', ttlSeconds: 60, queueTimeoutSeconds: 0 });
});

const pool = (name, platforms, ready = true) => ({ namespace: 'ci', name, ready, platforms });

test('validatePools: single pool needs no platforms', () => {
  const r = L.validatePools({ perAllocationEndpoints: false, pools: [pool('a', [])] }, [{ namespace: 'ci', pool: 'a' }]);
  assert.equal(r.length, 1);
});

test('validatePools rejects missing, unready, multi without allocation domain, empty or overlapping platforms', () => {
  const w = [{ namespace: 'ci', pool: 'a' }, { namespace: 'ci', pool: 'b' }];
  assert.throws(() => L.validatePools({ pools: [pool('a', ['linux/amd64'])] }, w), /not found/);
  assert.throws(() => L.validatePools({ pools: [pool('a', [], false)] }, [w[0]]), /not ready/);
  assert.throws(() => L.validatePools({ perAllocationEndpoints: false, pools: [pool('a', ['x']), pool('b', ['y'])] }, w), /allocationDomain/);
  assert.throws(() => L.validatePools({ perAllocationEndpoints: true, pools: [pool('a', ['x']), pool('b', [])] }, w), /no platforms/);
  assert.throws(() => L.validatePools({ perAllocationEndpoints: true, pools: [pool('a', ['x']), pool('b', ['x'])] }, w), /both/);
  assert.equal(L.validatePools({ perAllocationEndpoints: true, pools: [pool('a', ['x']), pool('b', ['y'])] }, w).length, 2);
});

test('buildxCommands: create, append per extra pool, use', () => {
  const n = (i, plat, ca) => ({ endpoint: `tcp://id${i}.alloc.bk:8444`, serverName: 'bk', certPath: `/d${i}/cert.pem`, keyPath: `/d${i}/key.pem`, caPath: ca, platforms: plat });
  const c = L.buildxCommands('bk-id0', [n(0, ['linux/amd64'], '/d0/ca.pem'), n(1, ['linux/arm64'])]);
  assert.deepEqual(c[0], ['buildx', 'create', '--name', 'bk-id0', '--node', 'n0', '--driver', 'remote', '--platform', 'linux/amd64',
    '--driver-opt', 'cacert=/d0/ca.pem,cert=/d0/cert.pem,key=/d0/key.pem,servername=bk', 'tcp://id0.alloc.bk:8444']);
  assert.deepEqual(c[1].slice(0, 6), ['buildx', 'create', '--name', 'bk-id0', '--append', '--node']);
  assert.ok(c[1].includes('n1') && c[1].includes('linux/arm64'));
  assert.ok(!c[1].join(' ').includes('cacert'));
  assert.deepEqual(c[2], ['buildx', 'use', 'bk-id0']);
  const single = L.buildxCommands('b', [n(0, [], undefined)]);
  assert.ok(!single[0].includes('--platform'));
});

test('writeMaterial writes 0600 files and omits ca when null', () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), 'bk-test-'));
  try {
    const m = L.writeMaterial(path.join(d, 'x'), { caCert: null, clientCert: 'CERT' }, 'KEY');
    assert.equal(m.caPath, undefined);
    assert.equal(fs.readFileSync(m.certPath, 'utf8'), 'CERT');
    assert.equal(fs.statSync(m.keyPath).mode & 0o777, 0o600);
  } finally { fs.rmSync(d, { recursive: true, force: true }); }
});

test('apiError never echoes unknown bodies', () => {
  assert.equal(L.apiError('x', { status: 500, body: 'PRIVATE KEY' }).message, 'x failed: HTTP 500');
  assert.match(L.apiError('x', { status: 400, body: '{"error":{"code":"bad_request","message":"bad ttl"}}' }).message, /bad_request: bad ttl/);
});

test('request round-trips JSON', async () => {
  const srv = http.createServer((req, res) => {
    const c = [];
    req.on('data', (d) => c.push(d));
    req.on('end', () => { res.writeHead(201); res.end(Buffer.concat(c)); });
  });
  await new Promise((r) => srv.listen(0, '127.0.0.1', r));
  try {
    const r = await L.request(`http://127.0.0.1:${srv.address().port}/v1/allocations`, { method: 'POST', body: { a: 1 } });
    assert.equal(r.status, 201);
    assert.equal(r.body, '{"a":1}');
  } finally { srv.close(); }
});

test('postTimeoutMs covers queue timeout + startTimeout ceiling + 10 s', () => {
  assert.equal(L.postTimeoutMs(300), (300 + 1800 + 10) * 1000);
  assert.equal(L.postTimeoutMs(0), 1810000);
  assert.equal(L.postTimeoutMs(undefined), (3600 + 1800 + 10) * 1000);
});

test('checkAllocationId rejects path traversal and odd shapes', () => {
  assert.equal(L.checkAllocationId('abc123'), 'abc123');
  for (const bad of ['x/../../..', '..', '', 'A1', 'a b', 'a'.repeat(64), undefined, 7, null]) {
    assert.throws(() => L.checkAllocationId(bad), /invalid allocation id/);
  }
});

test('createBuilder records the builder only after the first create succeeds', () => {
  const node = { certPath: 'c', keyPath: 'k', serverName: 's', endpoint: 'tcp://e:1' };
  const events = [];
  L.createBuilder('b', [node, node], (args) => events.push(args[1]), () => events.push('saved'));
  assert.deepEqual(events, ['create', 'saved', 'create', 'use']);

  const failed = [];
  assert.throws(() => L.createBuilder('b', [node], () => { throw new Error('exists'); }, () => failed.push('saved')));
  assert.deepEqual(failed, []);
});

test('parseCacheInputs: defaults, validation, one cache-from per line', () => {
  assert.deepEqual(L.parseCacheInputs({}), { cache: 'auto', cacheKey: undefined, mode: undefined, from: [] });
  assert.deepEqual(L.parseCacheInputs({ cache: 'registry', cacheKey: 'app-main', cacheMode: 'min', cacheFrom: 'type=registry,ref=r/x:a\n\n type=gha \n' }),
    { cache: 'registry', cacheKey: 'app-main', mode: 'min', from: ['type=registry,ref=r/x:a', 'type=gha'] });
  assert.throws(() => L.parseCacheInputs({ cache: 'all' }), /invalid cache/);
  for (const bad of ['-a', 'a/b', 'a-', 'x'.repeat(64)]) assert.throws(() => L.parseCacheInputs({ cacheKey: bad }), /invalid cache-key/);
  assert.equal(L.parseCacheInputs({ cacheKey: 'x'.repeat(63) }).cacheKey.length, 63);
  assert.throws(() => L.parseCacheInputs({ cacheMode: 'all' }), /invalid cache-mode/);
});

test('allocationBody sends cache and cacheKey, leaving out the auto default', () => {
  const p = { namespace: 'ci', pool: 'a' };
  assert.deepEqual(L.allocationBody(p, 'PK', undefined, undefined, L.parseCacheInputs({})), { namespace: 'ci', pool: 'a', publicKey: 'PK' });
  assert.deepEqual(L.allocationBody(p, 'PK', undefined, undefined, L.parseCacheInputs({ cache: 'none', cacheKey: 'k1' })),
    { namespace: 'ci', pool: 'a', publicKey: 'PK', cache: 'none', cacheKey: 'k1' });
  assert.deepEqual(L.allocationBody(p, 'PK', undefined, undefined, L.parseCacheInputs({ cacheKey: 'k1' })),
    { namespace: 'ci', pool: 'a', publicKey: 'PK', cacheKey: 'k1' });
});

test('cacheFlags: registry flags from responses, mode override, extra refs, dedup', () => {
  const to = 'type=registry,ref=r/c:app,mode=max,image-manifest=true,oci-mediatypes=true';
  const a = { cache: { key: 'app', registry: { to, from: ['type=registry,ref=r/c:app', 'type=registry,ref=r/c:trunk'] }, local: { result: 'hit' } } };
  const f = L.cacheFlags([a, a], { from: ['type=registry,ref=r/c:extra', 'type=registry,ref=r/c:app'] });
  assert.deepEqual(f.to, [to]);
  assert.deepEqual(f.from, ['type=registry,ref=r/c:app', 'type=registry,ref=r/c:trunk', 'type=registry,ref=r/c:extra']);
  assert.deepEqual(f.args, ['--cache-from=type=registry,ref=r/c:app', '--cache-from=type=registry,ref=r/c:trunk',
    '--cache-from=type=registry,ref=r/c:extra', `--cache-to=${to}`]);
  assert.deepEqual(L.cacheFlags([a], { mode: 'min' }).to, ['type=registry,ref=r/c:app,image-manifest=true,oci-mediatypes=true,mode=min']);
  // Local-only or no cache object: no registry flags, but extra refs still apply.
  const local = { cache: { key: 'app', local: { result: 'miss' } } };
  assert.deepEqual(L.cacheFlags([local, {}], {}), { to: [], from: [], args: [] });
  assert.deepEqual(L.cacheFlags([{}], { from: ['type=gha'] }).args, ['--cache-from=type=gha']);
});
