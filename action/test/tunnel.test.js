'use strict';
// RFC 6455 framing and the forwarder against a local WebSocket server (plain and TLS).
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const net = require('node:net');
const crypto = require('node:crypto');
const { once } = require('node:events');
const T = require('../dist/tunnel');
const { wsServer, frame } = require('./wsserver');
const { selfSigned, skipNoOpenssl: skip, sleep } = require('./helpers');

const { OP, encodeFrame, FrameParser, serve } = T;

/** A parser that records what it is handed. */
function recorder() {
  const r = { data: [], pings: [], pongs: [], closes: [], errors: [] };
  r.parser = new FrameParser({
    data: (b) => r.data.push(Buffer.from(b)),
    ping: (p) => r.pings.push(p),
    pong: (p) => r.pongs.push(p),
    close: (code, reason) => r.closes.push([code, reason]),
    error: (e) => r.errors.push(e.message),
  });
  r.bytes = () => Buffer.concat(r.data);
  return r;
}

/** Feed `buf` to the parser in pieces of `n` bytes. */
function feed(r, buf, n = buf.length) {
  for (let i = 0; i < buf.length; i += n) r.parser.push(buf.subarray(i, i + n));
}

test('a masked frame matches the RFC 6455 example bytes', () => {
  const f = encodeFrame(OP.TEXT, Buffer.from('Hello'), { maskKey: Buffer.from([0x37, 0xfa, 0x21, 0x3d]) });
  assert.equal(f.toString('hex'), '818537fa213d7f9f4d5158');
});

test('client frames are masked with a fresh key and the payload is not in the clear', () => {
  const payload = Buffer.from('a'.repeat(64));
  const a = encodeFrame(OP.BIN, payload);
  const b = encodeFrame(OP.BIN, payload);
  assert.equal(a[1] & 0x80, 0x80);
  assert.notDeepEqual(a.subarray(2, 6), b.subarray(2, 6), 'keys differ');
  assert.ok(!a.includes(payload));
  const key = a.subarray(2, 6);
  const plain = Buffer.from(a.subarray(6).map((x, i) => x ^ key[i & 3]));
  assert.deepEqual(plain, payload);
});

test('length forms: 7-bit up to 125, 16-bit from 126 to 65535, 64-bit from 65536', () => {
  const head = (n) => encodeFrame(OP.BIN, Buffer.alloc(n), { mask: false }).subarray(0, 10);
  assert.equal(head(125)[1], 125);
  assert.deepEqual([...head(126).subarray(1, 4)], [126, 0, 126]);
  assert.deepEqual([...head(65535).subarray(1, 4)], [126, 255, 255]);
  const h = head(65536);
  assert.equal(h[1], 127);
  assert.equal(h.readBigUInt64BE(2), 65536n);
  assert.equal(encodeFrame(OP.BIN, Buffer.alloc(65536), { mask: false }).length, 10 + 65536);
  assert.equal(encodeFrame(OP.BIN, Buffer.alloc(300)).length, 4 + 4 + 300);
});

test('the parser reads every length form, even fed one byte at a time', () => {
  const sizes = [0, 1, 125, 126, 300, 65535, 65536, 70000];
  const payloads = sizes.map((n) => crypto.randomBytes(n));
  for (const step of [1, 7, 1 << 20]) {
    const r = recorder();
    for (const p of payloads) feed(r, frame(OP.BIN, p), step);
    assert.deepEqual(r.bytes(), Buffer.concat(payloads), `step ${step}`);
    assert.deepEqual(r.errors, []);
  }
  // A 64-bit length that a shorter form could hold is legal.
  const r = recorder();
  feed(r, frame(OP.BIN, Buffer.from('xyz'), { lenForm: 8 }));
  feed(r, frame(OP.BIN, Buffer.from('uvw'), { lenForm: 2 }));
  assert.equal(r.bytes().toString(), 'xyzuvw');
});

test('a large frame is handed on in pieces, never held whole', () => {
  const r = recorder();
  const big = crypto.randomBytes(1 << 20);
  feed(r, frame(OP.BIN, big), 4096);
  assert.ok(r.data.length > 100);
  assert.deepEqual(r.bytes(), big);
});

test('fragmented messages join in order, with a control frame between the fragments', () => {
  const r = recorder();
  feed(r, Buffer.concat([
    frame(OP.BIN, Buffer.from('ab'), { fin: false }),
    frame(OP.PING, Buffer.from('mid')),
    frame(OP.CONT, Buffer.from('cd'), { fin: false }),
    frame(OP.CONT, Buffer.from('ef'), { fin: true }),
    frame(OP.BIN, Buffer.from('gh')),
  ]), 3);
  assert.equal(r.bytes().toString(), 'abcdefgh');
  assert.deepEqual(r.pings.map(String), ['mid']);
  assert.deepEqual(r.errors, []);
});

test('ping, pong and close frames are delivered whole', () => {
  const r = recorder();
  feed(r, Buffer.concat([
    frame(OP.PING, Buffer.from('p1')),
    frame(OP.PONG, Buffer.from('p2')),
    frame(OP.PING),
    frame(OP.CLOSE, Buffer.concat([Buffer.from([0x03, 0xe8]), Buffer.from('bye')])),
  ]), 1);
  assert.deepEqual(r.pings.map(String), ['p1', '']);
  assert.deepEqual(r.pongs.map(String), ['p2']);
  assert.deepEqual(r.closes, [[1000, 'bye']]);
  const empty = recorder();
  feed(empty, frame(OP.CLOSE));
  assert.deepEqual(empty.closes, [[1005, '']]);
});

test('protocol violations are errors, and the parser then ignores input', () => {
  const bad = {
    'text frame': frame(OP.TEXT, Buffer.from('hi')),
    'masked server frame': encodeFrame(OP.BIN, Buffer.from('hi')),
    'reserved bits': Buffer.from([0xc2, 0x00]),
    'reserved opcode': Buffer.from([0x83, 0x00]),
    'fragmented control frame': frame(OP.PING, Buffer.from('x'), { fin: false }),
    'long control frame': frame(OP.PING, Buffer.alloc(126)),
    'unexpected continuation': frame(OP.CONT, Buffer.from('x')),
    'close of one byte': frame(OP.CLOSE, Buffer.from([1])),
    'length past 2^53': Buffer.from([0x82, 127, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
  };
  for (const [name, buf] of Object.entries(bad)) {
    const r = recorder();
    feed(r, buf);
    feed(r, frame(OP.BIN, Buffer.from('after')));
    assert.equal(r.errors.length, 1, name);
    assert.equal(r.data.length, 0, name);
  }
  const r = recorder();
  feed(r, Buffer.concat([frame(OP.BIN, Buffer.from('a'), { fin: false }), frame(OP.BIN, Buffer.from('b'))]));
  assert.equal(r.errors.length, 1, 'new message inside a fragmented one');
});

// ---- the forwarder, in process ----

const collect = (sock) => {
  const chunks = [];
  sock.on('data', (c) => chunks.push(c));
  return () => Buffer.concat(chunks);
};
const until = async (fn, ms = 5000) => {
  const end = Date.now() + ms;
  while (!fn()) {
    if (Date.now() > end) assert.fail('timed out');
    await sleep(10);
  }
};
const connect = (port, allowHalfOpen = false) => new Promise((resolve, reject) => {
  const s = net.connect({ port, host: '127.0.0.1', allowHalfOpen }, () => resolve(s));
  s.once('error', reject);
});

async function setup(t, serverOpts = {}, serveOpts = {}) {
  const ws = await wsServer(serverOpts);
  const fwd = await serve([`${serverOpts.tls ? 'https' : 'http'}://127.0.0.1:${ws.port}/v1/tunnel`], serveOpts);
  t.after(() => { fwd.close(); ws.close(); });
  return { ws, fwd, port: fwd.ports[0] };
}

test('bytes echo both ways; the server sends pings, in several length forms, and gets pongs', async (t) => {
  let n = 0;
  const { ws, port } = await setup(t, {
    onConn: (c) => {
      c.pingEvery(20, Buffer.from('keepalive'));
      c.data = (b) => c.write(OP.BIN, b, { lenForm: [0, 2, 8][n++ % 3] === 0 ? undefined : [0, 2, 8][(n - 1) % 3] });
    },
  });
  const s = await connect(port);
  const got = collect(s);
  const msg = crypto.randomBytes(300 * 1024);
  s.write(msg);
  await until(() => got().length >= msg.length);
  assert.deepEqual(got(), msg);
  const [c] = ws.state.conns;
  assert.equal(c.bytes, msg.length);
  await until(() => c.pongs >= 2);
  assert.ok(c.pongPayloads.every((p) => p.toString() === 'keepalive'));
  assert.deepEqual(c.bad, []);
  s.destroy();
});

test('a server message in fragments, with a ping inside it, reaches the client as one stream', async (t) => {
  const { port } = await setup(t, {
    onConn: (c) => {
      c.write(OP.BIN, Buffer.from('ab'), { fin: false });
      c.write(OP.PING, Buffer.from('x'));
      c.write(OP.CONT, Buffer.from('cd'), { fin: false });
      c.write(OP.CONT, Buffer.from('ef'));
    },
  });
  const s = await connect(port);
  const got = collect(s);
  await until(() => got().length >= 6);
  assert.equal(got().toString(), 'abcdef');
  s.destroy();
});

test('the client half-closing becomes a close frame, and the server close ends the client', async (t) => {
  const { ws, port } = await setup(t, { onConn: (c) => { c.data = (b) => c.write(OP.BIN, b); } });
  const s = await connect(port, true);
  const got = collect(s);
  s.write('last words');
  s.end();
  await once(s, 'end');
  await once(s, 'close');
  assert.equal(got().toString(), 'last words');
  const [c] = ws.state.conns;
  assert.deepEqual(c.closes, [1000]);
  assert.deepEqual(c.bad, []);
});

test('a close frame from the server ends the local connection', async (t) => {
  const { ws, port } = await setup(t, { onConn: (c) => { c.write(OP.BIN, Buffer.from('hi')); c.write(OP.CLOSE, Buffer.from([0x03, 0xe8])); } });
  const s = await connect(port);
  const got = collect(s);
  await once(s, 'end');
  s.end();
  await once(s, 'close');
  assert.equal(got().toString(), 'hi');
  // The forwarder answered with a close of its own.
  await until(() => ws.state.conns[0].closes.length === 1);
  assert.deepEqual(ws.state.conns[0].closes, [1000]);
});

test('an EOF from the server without a close frame ends the local connection', async (t) => {
  const { port } = await setup(t, { onConn: (c) => { c.write(OP.BIN, Buffer.from('bye')); c.socket.end(); } });
  const s = await connect(port);
  const got = collect(s);
  await once(s, 'end');
  s.end();
  await once(s, 'close');
  assert.equal(got().toString(), 'bye');
});

test('an 8 MiB tail arrives intact under backpressure while the client writes', async (t) => {
  // 8 MiB tail plus a close frame; the client is paused so the tail queues in the forwarder's
  // write buffer, then the client writes after the close has ended the local side.
  const tail = crypto.randomBytes(8 * 1024 * 1024);
  const { port } = await setup(t, { onConn: (c) => { c.write(OP.BIN, tail); c.write(OP.CLOSE, Buffer.from([0x03, 0xe8])); } });
  const s = await connect(port, true);
  s.pause();
  await sleep(200);
  s.write('late');
  const chunks = [];
  s.on('data', (c) => chunks.push(c));
  s.resume();
  await once(s, 'end');
  assert.deepEqual(Buffer.concat(chunks), tail);
  s.end();
  await once(s, 'close');
});

test('a local connection that is reset sends close 1001', async (t) => {
  const { ws, port } = await setup(t);
  const s = await connect(port);
  await until(() => ws.state.conns.length === 1);
  s.on('error', () => {});
  s.resetAndDestroy();
  await until(() => ws.state.conns[0].closes.length === 1);
  assert.deepEqual(ws.state.conns[0].closes, [1001]);
});

test('a text frame from the server closes the tunnel with 1002 and drops the local side', async (t) => {
  const { ws, port } = await setup(t, { onConn: (c) => { c.write(OP.TEXT, Buffer.from('nope')); } });
  const s = await connect(port);
  await once(s, 'close');
  await until(() => ws.state.conns[0].closes.length === 1);
  assert.deepEqual(ws.state.conns[0].closes, [1002]);
});

test('every accepted connection opens its own WebSocket', async (t) => {
  const { ws, port } = await setup(t, { onConn: (c) => { c.data = (b) => c.write(OP.BIN, b); } });
  const [a, b] = [await connect(port), await connect(port)];
  const [ga, gb] = [collect(a), collect(b)];
  a.write('from a');
  b.write('from b');
  await until(() => ga().length === 6 && gb().length === 6);
  assert.equal(ga().toString(), 'from a');
  assert.equal(gb().toString(), 'from b');
  assert.equal(ws.state.upgrades, 2);
  a.destroy();
  b.destroy();
});

test('one forwarder serves several targets on separate ports', async (t) => {
  const mk = async (tag) => wsServer({ onConn: (c) => { c.data = () => c.write(OP.BIN, Buffer.from(tag)); } });
  const [x, y] = [await mk('X'), await mk('Y')];
  const fwd = await serve([x, y].map((w) => `http://127.0.0.1:${w.port}/v1/tunnel`));
  t.after(() => { fwd.close(); x.close(); y.close(); });
  assert.notEqual(fwd.ports[0], fwd.ports[1]);
  for (const [i, tag] of ['X', 'Y'].entries()) {
    const s = await connect(fwd.ports[i]);
    const got = collect(s);
    s.write('?');
    await until(() => got().length === 1);
    assert.equal(got().toString(), tag);
    s.destroy();
  }
});

test('a tunnel the server refuses drops the local connection', async (t) => {
  const { ws, port } = await setup(t, { refuse: true });
  const s = await connect(port);
  await once(s, 'close');
  assert.equal(ws.state.upgrades, 1);
});

test('nothing listening behind the tunnel URL drops the local connection', async (t) => {
  const fwd = await serve(['http://127.0.0.1:1/v1/tunnel']);
  t.after(() => fwd.close());
  const s = await connect(fwd.ports[0]);
  await once(s, 'close');
});

test('over TLS the forwarder trusts the given CA and only that', { skip }, async (t) => {
  const tls = selfSigned(t);
  const opts = { tls: { key: fs.readFileSync(tls.key), cert: fs.readFileSync(tls.crt) }, onConn: (c) => { c.data = (b) => c.write(OP.BIN, b); } };
  const ca = fs.readFileSync(tls.crt, 'utf8');
  const { port } = await setup(t, opts, { ca });
  const s = await connect(port);
  const got = collect(s);
  s.write('over tls');
  await until(() => got().length === 8);
  assert.equal(got().toString(), 'over tls');
  s.destroy();

  const ws2 = await wsServer(opts);
  const noCa = await serve([`https://127.0.0.1:${ws2.port}/v1/tunnel`]);
  t.after(() => { noCa.close(); ws2.close(); });
  const s2 = await connect(noCa.ports[0]);
  await once(s2, 'close');
  assert.equal(ws2.state.upgrades, 0, 'handshake refused before any upgrade');
});

test('close() drops live connections', async (t) => {
  const ws = await wsServer();
  const fwd = await serve([`http://127.0.0.1:${ws.port}/v1/tunnel`]);
  t.after(() => ws.close());
  const s = await connect(fwd.ports[0]);
  await until(() => ws.state.conns.length === 1);
  fwd.close();
  await once(s, 'close');
  await assert.rejects(connect(fwd.ports[0]));
});
