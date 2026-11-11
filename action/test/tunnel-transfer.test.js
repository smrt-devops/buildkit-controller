'use strict';
// 50 MiB each way through the forwarder as its own process. Backpressure is checked head on: with
// the far reader stalled, the writer is held back after a bounded amount (socket buffers, not 50 MiB),
// and when the reader resumes every byte arrives intact.
const test = require('node:test');
const assert = require('node:assert/strict');
const net = require('node:net');
const crypto = require('node:crypto');
const { once } = require('node:events');
const L = require('../dist/lib');
const { wsServer } = require('./wsserver');
const { sleep } = require('./helpers');

const MIB = 1024 * 1024;
const TOTAL = 50 * MIB;
// What may sit in kernel socket buffers and the forwarder while a reader is stalled.
const MAX_HELD = 20 * MIB;
const STALL_MS = 750;
const block = crypto.randomBytes(MIB);

async function forwarder(t, wsPort) {
  const f = await L.startForwarder({ urls: [`http://127.0.0.1:${wsPort}/v1/tunnel`] });
  t.after(() => L.stopForwarder(f.pid));
  return f;
}

/** Resolves true on the next 'drain' of `s`, false if none comes within STALL_MS. */
const drained = (s) => new Promise((resolve) => {
  const timer = setTimeout(() => { s.off('drain', on); resolve(false); }, STALL_MS);
  const on = () => { clearTimeout(timer); resolve(true); };
  s.once('drain', on);
});

test('50 MiB from the local side: held back while the server stalls, intact once it reads', async (t) => {
  let connected;
  const conn = new Promise((r) => { connected = r; });
  const ws = await wsServer({ onConn: (c) => { c.socket.pause(); connected(c); } });
  t.after(() => ws.close());
  const fwd = await forwarder(t, ws.port);
  const s = net.connect({ port: fwd.ports[0], host: '127.0.0.1', allowHalfOpen: true });
  const hash = crypto.createHash('sha256');
  let sent = 0;
  let held;
  while (sent < TOTAL) {
    hash.update(block);
    sent += MIB;
    if (s.write(block)) continue;
    const ok = await drained(s);
    if (ok) continue;
    // Stalled: no drain while the server is not reading.
    held = sent;
    break;
  }
  assert.ok(held !== undefined, 'the writer was held back');
  assert.ok(held < MAX_HELD, `${held} bytes went in before the writer was held`);
  const c = await conn;
  assert.ok(c.bytes < MAX_HELD);
  const resumed = once(s, 'drain');
  c.socket.resume();
  await resumed;
  while (sent < TOTAL) {
    hash.update(block);
    sent += MIB;
    if (!s.write(block)) await once(s, 'drain');
  }
  s.end();
  await once(s, 'end');
  assert.equal(c.bytes, TOTAL);
  assert.equal(c.digest(), hash.digest('hex'));
  assert.deepEqual(c.closes, [1000]);
  assert.deepEqual(c.bad, []);
});

test('50 MiB from the server: held back while the local reader stalls, intact once it reads', async (t) => {
  const sent = crypto.createHash('sha256');
  let written = 0;
  let held;
  let stalled;
  const stall = new Promise((r) => { stalled = r; });
  let release;
  const released = new Promise((r) => { release = r; });
  const ws = await wsServer({
    onConn: async (c) => {
      while (written < TOTAL) {
        const piece = block.subarray(0, 64 * 1024);
        sent.update(piece);
        written += piece.length;
        if (c.write(2, piece)) continue;
        const ok = await drained(c.socket);
        if (ok) continue;
        held = written;
        stalled();
        await released;
      }
      c.write(8, Buffer.from([0x03, 0xe8]));
    },
  });
  t.after(() => ws.close());
  const fwd = await forwarder(t, ws.port);
  const s = net.connect({ port: fwd.ports[0], host: '127.0.0.1' });
  s.pause();
  const got = crypto.createHash('sha256');
  let n = 0;
  s.on('data', (b) => { n += b.length; got.update(b); });
  await stall;
  assert.ok(held < MAX_HELD, `${held} bytes went out before the server was held`);
  release();
  s.resume();
  await once(s, 'end');
  assert.equal(n, TOTAL);
  assert.equal(got.digest('hex'), sent.digest('hex'));
});
