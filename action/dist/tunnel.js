'use strict';
// Local forwarder for the tunnel transport. buildx dials only tcp://, so this
// process listens on 127.0.0.1, opens one WebSocket per accepted connection and copies bytes both
// ways. Node built-ins only, with its own RFC 6455 framing. It holds no tokens or keys: the one
// secret-free input is the CA bundle for the WebSocket, in BUILDKIT_TUNNEL_CA.
//
//   node tunnel.js --url <https://host/v1/tunnel> [--url ...]
//
// One listener per --url. It prints one JSON line, {"ports":[...]}, once every listener is up, and
// exits on SIGTERM or SIGINT.
const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const net = require('node:net');

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
const OP = { CONT: 0, TEXT: 1, BIN: 2, CLOSE: 8, PING: 9, PONG: 10 };
const CHUNK = 64 * 1024;
const CLOSE_GRACE_MS = 5000;
const CONNECT_TIMEOUT_MS = 30000;

/** One frame as bytes. Client frames are masked (the default); `maskKey` is for tests. */
function encodeFrame(opcode, payload = Buffer.alloc(0), { mask = true, fin = true, maskKey } = {}) {
  const len = payload.length;
  const ext = len < 126 ? 0 : len <= 0xffff ? 2 : 8;
  const hl = 2 + ext + (mask ? 4 : 0);
  const out = Buffer.allocUnsafe(hl + len);
  out[0] = (fin ? 0x80 : 0) | opcode;
  if (ext === 0) out[1] = len;
  else if (ext === 2) { out[1] = 126; out.writeUInt16BE(len, 2); } else { out[1] = 127; out.writeBigUInt64BE(BigInt(len), 2); }
  if (!mask) {
    payload.copy(out, hl);
    return out;
  }
  out[1] |= 0x80;
  const key = maskKey || crypto.randomBytes(4);
  key.copy(out, 2 + ext);
  for (let i = 0; i < len; i++) out[hl + i] = payload[i] ^ key[i & 3];
  return out;
}

function closePayload(code) {
  const p = Buffer.alloc(2);
  p.writeUInt16BE(code);
  return p;
}

/**
 * Streaming decoder for frames from a server. Data payloads (binary and continuation) go to
 * `data(chunk)` as they arrive, so memory stays bounded however large a frame is; control frames
 * (at most 125 bytes) are buffered and delivered whole. Handlers: data, ping, pong, close(code,
 * reason), error(err). After an error the parser ignores input.
 */
class FrameParser {
  constructor(h) {
    this.h = h;
    this.carry = null;
    this.cur = null;
    this.fragmenting = false;
    this.dead = false;
  }

  fail(msg) {
    this.dead = true;
    this.h.error(new Error(`websocket protocol error: ${msg}`));
  }

  /** Parse a header at `buf[off]`; undefined when incomplete, null after a failure. */
  header(buf, off) {
    if (buf.length - off < 2) return undefined;
    const b0 = buf[off];
    const b1 = buf[off + 1];
    const opcode = b0 & 0x0f;
    const fin = (b0 & 0x80) !== 0;
    let len = b1 & 0x7f;
    let size = 2;
    if (len === 126) {
      if (buf.length - off < 4) return undefined;
      len = buf.readUInt16BE(off + 2);
      size = 4;
    } else if (len === 127) {
      if (buf.length - off < 10) return undefined;
      const big = buf.readBigUInt64BE(off + 2);
      if (big > BigInt(Number.MAX_SAFE_INTEGER)) { this.fail('frame length out of range'); return null; }
      len = Number(big);
      size = 10;
    }
    if (b0 & 0x70) { this.fail('reserved bits set'); return null; }
    if (b1 & 0x80) { this.fail('server frame is masked'); return null; }
    const control = opcode >= 8;
    if (opcode > OP.BIN && !control) { this.fail(`reserved opcode ${opcode}`); return null; }
    if (control && (!fin || len > 125 || ![OP.CLOSE, OP.PING, OP.PONG].includes(opcode))) { this.fail('bad control frame'); return null; }
    if (opcode === OP.TEXT) { this.fail('text frame'); return null; }
    if (opcode === OP.BIN && this.fragmenting) { this.fail('new message inside a fragmented one'); return null; }
    if (opcode === OP.CONT && !this.fragmenting) { this.fail('unexpected continuation'); return null; }
    return { opcode, fin, len, size, got: 0, chunks: [] };
  }

  finish(f) {
    if (f.opcode === OP.BIN || f.opcode === OP.CONT) { this.fragmenting = !f.fin; return; }
    const p = Buffer.concat(f.chunks);
    if (f.opcode === OP.PING) this.h.ping(p);
    else if (f.opcode === OP.PONG) this.h.pong(p);
    else if (p.length === 1) this.fail('close payload of one byte');
    else this.h.close(p.length ? p.readUInt16BE(0) : 1005, p.subarray(2).toString('utf8'));
  }

  push(chunk) {
    const buf = this.carry ? Buffer.concat([this.carry, chunk]) : chunk;
    this.carry = null;
    let off = 0;
    while (off < buf.length && !this.dead) {
      if (!this.cur) {
        const f = this.header(buf, off);
        if (f === undefined) { this.carry = Buffer.from(buf.subarray(off)); return; }
        if (f === null) return;
        off += f.size;
        this.cur = f;
        if (f.len === 0) { this.cur = null; this.finish(f); continue; }
      }
      const f = this.cur;
      const piece = buf.subarray(off, off + Math.min(buf.length - off, f.len - f.got));
      off += piece.length;
      f.got += piece.length;
      if (f.opcode === OP.BIN || f.opcode === OP.CONT) this.h.data(piece);
      else f.chunks.push(piece);
      if (f.got === f.len) { this.cur = null; this.finish(f); }
    }
  }
}

/**
 * Open a WebSocket with node:http(s) upgrade. `url` may use http(s) or ws(s); TLS is used for https
 * and wss, trusting `ca` when given. Resolves {socket, head}: the upgraded socket and any bytes the
 * server sent after its 101.
 */
function openWebSocket(url, { ca, signal, timeoutMs = CONNECT_TIMEOUT_MS } = {}) {
  return new Promise((resolve, reject) => {
    const u = new URL(url);
    const secure = u.protocol === 'https:' || u.protocol === 'wss:';
    const key = crypto.randomBytes(16).toString('base64');
    const opts = {
      hostname: u.hostname.replace(/^\[|\]$/g, ''),
      port: u.port || (secure ? 443 : 80),
      path: u.pathname + u.search,
      method: 'GET',
      agent: false,
      timeout: timeoutMs,
      headers: { Connection: 'Upgrade', Upgrade: 'websocket', 'Sec-WebSocket-Version': '13', 'Sec-WebSocket-Key': key },
    };
    if (ca) opts.ca = ca;
    if (signal) opts.signal = signal;
    const req = (secure ? https : http).request(opts);
    req.on('upgrade', (res, socket, head) => {
      const want = crypto.createHash('sha1').update(key + GUID).digest('base64');
      if (res.headers['sec-websocket-accept'] !== want) {
        socket.destroy();
        reject(new Error('tunnel answered with a wrong Sec-WebSocket-Accept'));
        return;
      }
      socket.setTimeout(0);
      resolve({ socket, head });
    });
    req.on('response', (res) => {
      res.resume();
      req.destroy();
      reject(new Error(`tunnel refused: HTTP ${res.statusCode}`));
    });
    req.on('timeout', () => req.destroy(new Error('tunnel connect timed out')));
    req.on('error', reject);
    req.end();
  });
}

/**
 * Copy bytes between an accepted local socket and one tunnel. Both directions honour backpressure:
 * a full write buffer pauses the reading side until 'drain'. A half-close of the local side is a
 * close frame; a close frame or EOF from the tunnel ends the local side.
 */
function bridge(local, { socket, head }, { closeGraceMs = CLOSE_GRACE_MS } = {}) {
  let closeSent = false;
  let finished = false;
  let timer;
  const finish = () => {
    if (finished) return;
    finished = true;
    clearTimeout(timer);
    local.destroy();
    socket.destroy();
  };
  // Both sides gone: nothing is left for the grace timer to cut.
  const settle = () => { if (local.destroyed && socket.destroyed) clearTimeout(timer); };
  const grace = () => { if (!timer && !finished) timer = setTimeout(finish, closeGraceMs); };
  const send = (op, payload) => { if (socket.writable) socket.write(encodeFrame(op, payload)); };
  const sendClose = (code) => {
    if (closeSent) return;
    closeSent = true;
    send(OP.CLOSE, closePayload(code));
  };
  // The tunnel's stream is over: flush what is queued to the local side and let it end.
  const tunnelEnded = () => { local.end(); grace(); };

  const parser = new FrameParser({
    data: (b) => { if (local.writable && !local.write(b)) socket.pause(); },
    ping: (p) => send(OP.PONG, p),
    pong: () => {},
    close: (code) => {
      sendClose(code === 1005 ? 1000 : code);
      socket.end();
      tunnelEnded();
    },
    error: () => { sendClose(1002); socket.end(); local.destroy(); grace(); },
  });

  local.on('drain', () => socket.resume());
  socket.on('drain', () => local.resume());
  local.on('data', (chunk) => {
    // After the close is sent or the tunnel is not writable, late local bytes are dropped; a write
    // here would raise ERR_STREAM_WRITE_AFTER_END and finish() would discard the tail still queued for local.
    if (closeSent || !socket.writable) return;
    for (let i = 0; i < chunk.length; i += CHUNK) {
      if (!socket.write(encodeFrame(OP.BIN, chunk.subarray(i, i + CHUNK)))) local.pause();
    }
  });
  local.on('end', () => { sendClose(1000); grace(); });
  local.on('error', () => {});
  local.on('close', () => { sendClose(1001); socket.end(); grace(); settle(); });
  socket.on('data', (b) => parser.push(b));
  socket.on('end', tunnelEnded);
  socket.on('error', finish);
  socket.on('close', () => { tunnelEnded(); settle(); });
  if (head && head.length) parser.push(head);
  local.resume();
}

/**
 * Start one listener per target URL on `host` (port picked by the kernel). Resolves
 * {ports, close()}; `close` stops the listeners and drops every connection. `log(url, err)` hears
 * each failed WebSocket open.
 */
async function serve(urls, { ca, host = '127.0.0.1', closeGraceMs, log = () => {} } = {}) {
  const conns = new Set();
  const track = (s) => { conns.add(s); s.once('close', () => conns.delete(s)); };
  const servers = urls.map((url) => net.createServer({ allowHalfOpen: true, pauseOnConnect: true }, (local) => {
    track(local);
    local.on('error', () => {});
    const ac = new AbortController();
    local.once('close', () => ac.abort());
    openWebSocket(url, { ca, signal: ac.signal }).then((ws) => {
      track(ws.socket);
      if (local.destroyed) { ws.socket.destroy(); return; }
      bridge(local, ws, { closeGraceMs });
    }, (e) => { log(url, e); local.destroy(); });
  }));
  const ports = await Promise.all(servers.map((s) => new Promise((resolve, reject) => {
    s.once('error', reject);
    s.listen(0, host, () => resolve(s.address().port));
  })));
  return {
    ports,
    close() {
      for (const s of servers) s.close();
      for (const c of conns) c.destroy();
    },
  };
}

function main(argv) {
  const urls = [];
  for (let i = 0; i < argv.length; i += 2) {
    if (argv[i] !== '--url' || !argv[i + 1]) throw new Error('usage: tunnel.js --url <url> [--url <url> ...]');
    urls.push(argv[i + 1]);
  }
  if (urls.length === 0) throw new Error('usage: tunnel.js --url <url> [--url <url> ...]');
  // The parent drops its end of these pipes after the first line; a later write must not kill us.
  process.stdout.on('error', () => {});
  process.stderr.on('error', () => {});
  // One line per failed WebSocket open: time, target URL, HTTP status or error code. The file is the
  // only record, because the parent closes this process's stderr. Nothing secret reaches it.
  const logFile = process.env.BUILDKIT_TUNNEL_LOG;
  const log = (url, e) => {
    if (!logFile) return;
    const what = String((e && e.message) || e).replace(/\s+/g, ' ');
    try { fs.appendFileSync(logFile, `${new Date().toISOString()} ${url} ${e && e.code ? `${e.code} ` : ''}${what}\n`, { mode: 0o600 }); } catch { /* the log is best effort */ }
  };
  return serve(urls, { ca: process.env.BUILDKIT_TUNNEL_CA || undefined, log }).then((s) => {
    const stop = () => { s.close(); process.exit(0); };
    process.on('SIGTERM', stop);
    process.on('SIGINT', stop);
    process.stdout.write(`${JSON.stringify({ ports: s.ports })}\n`);
  });
}

if (require.main === module) {
  main(process.argv.slice(2)).catch((e) => {
    process.stderr.write(`${e.message}\n`);
    process.exit(1);
  });
}

module.exports = { OP, encodeFrame, FrameParser, openWebSocket, bridge, serve };
