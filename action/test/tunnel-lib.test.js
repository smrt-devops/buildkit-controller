'use strict';
// Starting and stopping the forwarder through lib.js, and mapping tunnel endpoints to local ports.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');
const L = require('../dist/lib');
const { sleep } = require('./helpers');

const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };
const waitDead = async (pid) => { for (let i = 0; i < 200 && alive(pid); i++) await sleep(10); return !alive(pid); };
const canConnect = (port) => new Promise((resolve) => {
  const s = net.connect({ port, host: '127.0.0.1' }, () => { s.destroy(); resolve(true); });
  s.on('error', () => resolve(false));
});
function scratch(t) {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), 'bk-fwd-'));
  t.after(() => fs.rmSync(d, { recursive: true, force: true }));
  return d;
}
const fakeScript = (t, body) => {
  const f = path.join(scratch(t), 'fake.js');
  fs.writeFileSync(f, body);
  return f;
};

test('isTunnelEndpoint is true only for a path', () => {
  assert.equal(L.isTunnelEndpoint('/v1/tunnel'), true);
  assert.equal(L.isTunnelEndpoint('tcp://e.example:8444'), false);
  assert.equal(L.isTunnelEndpoint(''), false);
  assert.equal(L.isTunnelEndpoint(undefined), false);
});

test('tunnelUrl appends the path to the API base URL', () => {
  assert.equal(L.tunnelUrl('https://bk.example.com:8443', '/v1/tunnel'), 'https://bk.example.com:8443/v1/tunnel');
  assert.equal(L.tunnelUrl('https://bk.example.com/base', '/v1/tunnel'), 'https://bk.example.com/base/v1/tunnel');
});

test('useTunnels gives each tunnel node its own local port and leaves tcp nodes alone', async () => {
  const nodes = [
    { endpoint: '/v1/tunnel', serverName: 's', platforms: ['linux/a'] },
    { endpoint: 'tcp://x.example:1', serverName: 's' },
    { endpoint: '/v1/tunnel', serverName: 's', platforms: ['linux/b'] },
  ];
  const started = [];
  const out = await L.useTunnels(nodes, {
    api: 'https://bk.example.com', ca: 'PEM', logFile: '/x/fwd.log',
    start: async (a) => { started.push(a); return { pid: 4242, ports: [5001, 5002] }; },
    onStarted: (pid) => started.push(pid),
  });
  assert.deepEqual(started, [{ urls: ['https://bk.example.com/v1/tunnel', 'https://bk.example.com/v1/tunnel'], ca: 'PEM', logFile: '/x/fwd.log' }, 4242]);
  assert.deepEqual(out.map((n) => n.endpoint), ['tcp://127.0.0.1:5001', 'tcp://x.example:1', 'tcp://127.0.0.1:5002']);
  assert.deepEqual(out[0].platforms, ['linux/a']);
  assert.equal(nodes[0].endpoint, '/v1/tunnel', 'input is not modified');
});

test('useTunnels starts nothing when every endpoint is tcp://', async () => {
  const nodes = [{ endpoint: 'tcp://x.example:1' }];
  const out = await L.useTunnels(nodes, { api: 'https://a', start: async () => assert.fail('started') });
  assert.equal(out, nodes);
});

test('startForwarder runs dist/tunnel.js detached, reports its ports, and stopForwarder ends it', async (t) => {
  const f = await L.startForwarder({ urls: ['http://127.0.0.1:1/v1/tunnel', 'http://127.0.0.1:1/v1/tunnel'] });
  t.after(() => L.stopForwarder(f.pid));
  assert.equal(f.ports.length, 2);
  assert.notEqual(f.ports[0], f.ports[1]);
  assert.ok(alive(f.pid));
  assert.ok(await canConnect(f.ports[0]));
  assert.equal(L.stopForwarder(f.pid), true);
  assert.ok(await waitDead(f.pid));
  assert.equal(await canConnect(f.ports[0]), false);
  assert.equal(L.stopForwarder(f.pid), false, 'a missing process is not an error');
});

test('stopForwarder ignores state that is not a pid', () => {
  for (const v of ['', undefined, 'abc', '0', '1', '-5', '1.5']) assert.equal(L.stopForwarder(v), false, String(v));
});

test('the forwarder gets the CA and none of the runner environment, so no token or key', async (t) => {
  const out = path.join(scratch(t), 'env.json');
  const script = fakeScript(t, `require('fs').writeFileSync(${JSON.stringify(out)}, JSON.stringify(process.env));
process.stdout.write('{"ports":[6001]}\\n'); setInterval(() => {}, 1000);`);
  process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN = 'secret-bearer';
  const tracking = process.env.RUNNER_TRACKING_ID;
  delete process.env.RUNNER_TRACKING_ID;
  t.after(() => { delete process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN; if (tracking !== undefined) process.env.RUNNER_TRACKING_ID = tracking; });
  const f = await L.startForwarder({ urls: ['https://a/v1/tunnel'], ca: 'CA PEM', script });
  t.after(() => L.stopForwarder(f.pid));
  const env = JSON.parse(fs.readFileSync(out, 'utf8'));
  assert.equal(env.BUILDKIT_TUNNEL_CA, 'CA PEM');
  assert.ok(!JSON.stringify(env).includes('secret-bearer'));
  assert.deepEqual(Object.keys(env).filter((k) => /^(ACTIONS|GITHUB|RUNNER|INPUT|STATE)_/.test(k)), []);
});

test('the forwarder gets RUNNER_TRACKING_ID when the runner sets it, and nothing else besides the CA', async (t) => {
  const out = path.join(scratch(t), 'env.json');
  const script = fakeScript(t, `require('fs').writeFileSync(${JSON.stringify(out)}, JSON.stringify(process.env));
process.stdout.write('{"ports":[6002]}\\n'); setInterval(() => {}, 1000);`);
  const tracking = process.env.RUNNER_TRACKING_ID;
  process.env.RUNNER_TRACKING_ID = 'track-7';
  process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN = 'secret-bearer';
  t.after(() => {
    delete process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
    if (tracking === undefined) delete process.env.RUNNER_TRACKING_ID; else process.env.RUNNER_TRACKING_ID = tracking;
  });
  const f = await L.startForwarder({ urls: ['https://a/v1/tunnel'], ca: 'CA PEM', script });
  t.after(() => L.stopForwarder(f.pid));
  const env = JSON.parse(fs.readFileSync(out, 'utf8'));
  assert.equal(env.RUNNER_TRACKING_ID, 'track-7');
  assert.equal(env.BUILDKIT_TUNNEL_CA, 'CA PEM');
  // macOS adds __CF_USER_TEXT_ENCODING to every child process; it is not passed by the parent.
  assert.deepEqual(Object.keys(env).filter((k) => k !== '__CF_USER_TEXT_ENCODING').sort(), ['BUILDKIT_TUNNEL_CA', 'RUNNER_TRACKING_ID']);
  assert.ok(!JSON.stringify(env).includes('secret-bearer'));
});

test('the forwarder outlives the process that started it', async (t) => {
  const { spawnSync } = require('node:child_process');
  const starter = `const L = require(${JSON.stringify(path.join(__dirname, '..', 'dist', 'lib.js'))});
L.startForwarder({ urls: ['http://127.0.0.1:1/v1/tunnel'] }).then((f) => console.log(JSON.stringify(f)));`;
  const r = spawnSync(process.execPath, ['-e', starter], { encoding: 'utf8', timeout: 20000 });
  assert.equal(r.status, 0, r.stderr);
  const f = JSON.parse(r.stdout);
  t.after(() => L.stopForwarder(f.pid));
  await sleep(100);
  assert.ok(alive(f.pid));
  assert.ok(await canConnect(f.ports[0]));
});

test('startForwarder fails with the child output when it exits early, prints junk, or never answers', async (t) => {
  const early = fakeScript(t, "process.stderr.write('boom: bad flag\\n'); process.exit(3);");
  await assert.rejects(L.startForwarder({ urls: ['https://a/x'], script: early }), /exited with 3: boom: bad flag/);
  const junk = fakeScript(t, "process.stdout.write('hello\\n'); setInterval(() => {}, 1000);");
  await assert.rejects(L.startForwarder({ urls: ['https://a/x'], script: junk }), /unreadable port line/);
  const wrong = fakeScript(t, "process.stdout.write('{\"ports\":[1,2]}\\n'); setInterval(() => {}, 1000);");
  await assert.rejects(L.startForwarder({ urls: ['https://a/x'], script: wrong }), /unreadable port line/);
  const pidFile = path.join(scratch(t), 'pid');
  const silent = fakeScript(t, `require('fs').writeFileSync(${JSON.stringify(pidFile)}, String(process.pid)); setInterval(() => {}, 1000);`);
  await assert.rejects(L.startForwarder({ urls: ['https://a/x'], script: silent, timeoutMs: 300 }), /did not start in time/);
  assert.ok(await waitDead(Number(fs.readFileSync(pidFile, 'utf8'))), 'the child that never answered is killed');
});

test('the forwarder appends one line per failed open to its log file: time, URL, error', async (t) => {
  const logFile = path.join(scratch(t), 'fwd.log');
  const url = 'http://127.0.0.1:1/v1/tunnel';
  const f = await L.startForwarder({ urls: [url], logFile });
  t.after(() => L.stopForwarder(f.pid));
  assert.ok(!fs.existsSync(logFile), 'nothing is written before a failure');
  const s = net.connect({ port: f.ports[0], host: '127.0.0.1' });
  s.on('error', () => {});
  for (let i = 0; i < 200 && !fs.existsSync(logFile); i++) await sleep(20);
  s.destroy();
  const lines = fs.readFileSync(logFile, 'utf8').trimEnd().split('\n');
  assert.equal(lines.length, 1);
  assert.match(lines[0], new RegExp(`^\\d{4}-\\d\\d-\\d\\dT[\\d:.]+Z ${url.replace(/[./]/g, '\\$&')} ECONNREFUSED`));
});
