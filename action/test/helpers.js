'use strict';
// Shared by the orchestration and recipe tests: a local HTTPS controller, a local OIDC endpoint,
// a fake docker on PATH, and a runner for the Action's scripts.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const https = require('node:https');
const { spawn, spawnSync } = require('node:child_process');

const have = (cmd) => spawnSync(cmd, ['--version']).status === 0 || spawnSync('which', [cmd]).status === 0;
/** Reason to skip the tests that need a real TLS certificate, or false. */
const skipNoOpenssl = have('openssl') ? false : 'openssl not available';

function listen(srv) {
  return new Promise((r) => srv.listen(0, '127.0.0.1', () => r(srv.address().port)));
}

/**
 * Controller + OIDC servers. `deny` pools answer 403, `deleteStatus` is the release answer,
 * `dockerFails` makes `docker buildx create` fail, and `override(rec)` may answer a request itself
 * with `{status, headers, body}`, or cut the connection with `{reset: true}`. `calls` records every request.
 */
async function fixture(t, { deny = [], deleteStatus = 204, dockerFails = false, override } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bk-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const key = path.join(dir, 'k.pem');
  const crt = path.join(dir, 'c.pem');
  const gen = spawnSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
    '-keyout', key, '-out', crt, '-subj', '/CN=localhost', '-addext', 'subjectAltName=IP:127.0.0.1', '-days', '1']);
  assert.equal(gen.status, 0, String(gen.stderr));
  const calls = [];
  let n = 0;
  const api = https.createServer({ key: fs.readFileSync(key), cert: fs.readFileSync(crt) }, (req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const body = Buffer.concat(chunks).toString();
      const rec = { method: req.method, url: req.url, body: body ? JSON.parse(body) : undefined, at: Date.now(), auth: req.headers.authorization };
      calls.push(rec);
      const reply = (status, obj, headers = {}) => {
        res.writeHead(status, { 'Content-Type': 'application/json', ...headers });
        res.end(typeof obj === 'string' ? obj : JSON.stringify(obj));
      };
      const o = override && override(rec, calls);
      if (o && o.reset) return req.socket.destroy();
      if (o) return reply(o.status, o.body ?? '', o.headers);
      if (req.method === 'GET' && req.url === '/v1/pools') {
        return reply(200, {
          perAllocationEndpoints: true,
          pools: ['a', 'b'].map((name) => ({ namespace: 'ci', name, ready: true, platforms: [`linux/${name}`] })),
        });
      }
      if (req.method === 'POST' && req.url === '/v1/allocations') {
        if (deny.includes(rec.body.pool)) return reply(403, { error: { code: 'forbidden', message: 'no' } });
        n += 1;
        return reply(201, {
          id: `id${n}${rec.body.pool}`, namespace: 'ci', pool: rec.body.pool, clientCert: 'CERT', endpoint: 'tcp://e.example:8444', serverName: 'e.example',
        });
      }
      if (req.method === 'DELETE') {
        res.writeHead(deleteStatus);
        return res.end();
      }
      return reply(404, {});
    });
  });
  const oidc = http.createServer((req, res) => { res.end(JSON.stringify({ value: 'oidc-token' })); });
  const [apiPort, oidcPort] = [await listen(api), await listen(oidc)];
  t.after(() => { api.close(); oidc.close(); });
  const bin = path.join(dir, 'bin');
  fs.mkdirSync(bin);
  const dockerLog = path.join(dir, 'docker.log');
  fs.writeFileSync(path.join(bin, 'docker'),
    `#!/bin/sh\necho "$*" >> '${dockerLog}'\n${dockerFails ? 'case "$*" in "buildx create"*) exit 1;; esac\n' : ''}exit 0\n`, { mode: 0o755 });
  const env = {
    PATH: `${bin}:${process.env.PATH}`,
    RUNNER_TEMP: path.join(dir, 'tmp'),
    GITHUB_STATE: path.join(dir, 'state'),
    GITHUB_OUTPUT: path.join(dir, 'output'),
    ACTIONS_ID_TOKEN_REQUEST_URL: `http://127.0.0.1:${oidcPort}/token`,
    ACTIONS_ID_TOKEN_REQUEST_TOKEN: 'bearer',
    INPUT_URL: `https://127.0.0.1:${apiPort}`,
    'INPUT_CA-CERT': fs.readFileSync(crt, 'utf8'),
    INPUT_AUDIENCE: 'buildkit-controller',
  };
  fs.mkdirSync(env.RUNNER_TEMP);
  const docker = () => (fs.existsSync(dockerLog) ? fs.readFileSync(dockerLog, 'utf8').trim().split('\n') : []);
  return { env, calls, docker, dir, caFile: crt };
}

/** Run a script to completion; resolves {code, out}. */
function run(file, args, env) {
  return new Promise((resolve) => {
    const p = spawn(file, args, { env });
    let out = '';
    p.stdout.on('data', (c) => { out += c; });
    p.stderr.on('data', (c) => { out += c; });
    p.on('close', (code, signal) => resolve({ code, signal, out }));
  });
}

/** Parse the `key<<delim ... delim` records of the GITHUB_STATE file. */
function readState(file) {
  const state = {};
  if (!fs.existsSync(file)) return state;
  const lines = fs.readFileSync(file, 'utf8').split('\n');
  for (let i = 0; i < lines.length; i++) {
    const m = /^(.+)<<(ghadelim_.+)$/.exec(lines[i]);
    if (!m) continue;
    const val = [];
    for (i += 1; lines[i] !== m[2]; i++) val.push(lines[i]);
    state[m[1]] = val.join('\n');
  }
  return state;
}

const deletes = (calls) => calls.filter((c) => c.method === 'DELETE').map((c) => c.url);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

module.exports = { fixture, run, readState, deletes, sleep, skipNoOpenssl, have };
