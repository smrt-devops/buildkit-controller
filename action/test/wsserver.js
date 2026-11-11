'use strict';
// A WebSocket server for the tunnel tests, built from node:http(s) and its own frame code (not
// dist/tunnel.js), so the tests do not check the forwarder against itself.
const crypto = require('node:crypto');
const http = require('node:http');
const https = require('node:https');

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

/** A server-to-client frame (unmasked). `lenForm` forces 2 or 8 length bytes where the payload would allow fewer. */
function frame(op, payload = Buffer.alloc(0), { fin = true, lenForm } = {}) {
  const n = payload.length;
  const form = lenForm || (n < 126 ? 0 : n < 65536 ? 2 : 8);
  const head = Buffer.alloc(2 + form);
  head[0] = (fin ? 0x80 : 0) | op;
  if (form === 0) head[1] = n;
  else if (form === 2) { head[1] = 126; head.writeUInt16BE(n, 2); } else { head[1] = 127; head.writeBigUInt64BE(BigInt(n), 2); }
  return Buffer.concat([head, payload]);
}

/** Decode client frames (masked) as they stream in. Emits data(chunk), ping(buf), pong(buf), close(code), bad(msg). */
function decoder(h) {
  let hdr = Buffer.alloc(0);
  let cur = null;
  return (chunk) => {
    let buf = chunk;
    while (buf.length) {
      if (!cur) {
        hdr = Buffer.concat([hdr, buf]);
        buf = Buffer.alloc(0);
        if (hdr.length < 2) return;
        const masked = (hdr[1] & 0x80) !== 0;
        let len = hdr[1] & 0x7f;
        let at = 2;
        if (len === 126) { if (hdr.length < 4) return; len = hdr.readUInt16BE(2); at = 4; } else if (len === 127) { if (hdr.length < 10) return; len = Number(hdr.readBigUInt64BE(2)); at = 10; }
        if (!masked) return h.bad('client frame is not masked');
        if (hdr.length < at + 4) return;
        cur = { op: hdr[0] & 0x0f, fin: (hdr[0] & 0x80) !== 0, key: hdr.subarray(at, at + 4), left: len, ctl: [], seen: 0 };
        buf = hdr.subarray(at + 4);
        hdr = Buffer.alloc(0);
        if (len === 0) { done(cur, h); cur = null; continue; }
      }
      const take = buf.subarray(0, Math.min(buf.length, cur.left));
      const out = Buffer.allocUnsafe(take.length);
      for (let i = 0; i < take.length; i++) out[i] = take[i] ^ cur.key[(cur.seen + i) & 3];
      cur.seen += take.length;
      cur.left -= take.length;
      buf = buf.subarray(take.length);
      if (cur.op === 2 || cur.op === 0) h.data(out); else cur.ctl.push(out);
      if (cur.left === 0) { done(cur, h); cur = null; }
    }
  };
}

function done(f, h) {
  const p = Buffer.concat(f.ctl);
  if (f.op === 9) h.ping(p);
  else if (f.op === 10) h.pong(p);
  else if (f.op === 8) h.close(p.length >= 2 ? p.readUInt16BE(0) : 1005);
}

/**
 * Start a server on 127.0.0.1. `tls` ({key, cert} PEM strings) makes it https. For each upgraded
 * connection `onConn(c)` runs, where c has: `bytes`, `hash` (sha256 of the data received, `digest()`),
 * `pongs`, `closes` (codes), `write(op, payload, opts)`, `data` (set a handler), `socket`, `pingEvery(ms)`.
 * By default the server answers a close frame with a close frame and ends. `upgrades` counts requests
 * that reached the upgrade; `refuse` makes the server answer 426 instead.
 */
function wsServer({ tls, onConn = () => {}, refuse = false } = {}) {
  const state = { upgrades: 0, conns: [] };
  const srv = (tls ? https.createServer(tls) : http.createServer()).on('request', (req, res) => { res.writeHead(426); res.end(); });
  srv.on('upgrade', (req, socket) => {
    state.upgrades += 1;
    if (refuse) { socket.end('HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'); return; }
    const accept = crypto.createHash('sha1').update(req.headers['sec-websocket-key'] + GUID).digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    const hash = crypto.createHash('sha256');
    const c = {
      socket, req, bytes: 0, pongs: 0, pings: 0, closes: [], bad: [], pongPayloads: [], data: null, timer: null,
      digest: () => hash.copy().digest('hex'),
      write: (op, payload, opts) => socket.write(frame(op, payload, opts)),
      pingEvery(ms, payload = Buffer.from('p')) { c.timer = setInterval(() => c.write(9, payload), ms); },
    };
    state.conns.push(c);
    const dec = decoder({
      data: (b) => { c.bytes += b.length; hash.update(b); if (c.data) c.data(b); },
      ping: (p) => { c.pings += 1; c.write(10, p); },
      pong: (p) => { c.pongs += 1; c.pongPayloads.push(p); },
      close: (code) => { c.closes.push(code); c.write(8, Buffer.from([code >> 8, code & 255])); socket.end(); },
      bad: (m) => { c.bad.push(m); socket.destroy(); },
    });
    socket.on('data', dec);
    socket.on('error', () => {});
    socket.on('close', () => clearInterval(c.timer));
    onConn(c);
  });
  return new Promise((resolve) => srv.listen(0, '127.0.0.1', () => resolve({
    port: srv.address().port, state, srv,
    close: () => { for (const c of state.conns) { clearInterval(c.timer); c.socket.destroy(); } srv.close(); },
  })));
}

module.exports = { wsServer, frame };
