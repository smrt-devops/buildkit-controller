'use strict';
// Pure helpers and a tiny HTTP client. Node built-ins only.
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const https = require('node:https');

/** Split a comma/newline list, trim, drop empties. */
function parseList(s) {
  return String(s || '').split(/[\n,]/).map((x) => x.trim()).filter(Boolean);
}

const DNS_LABEL = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?$/;

/** "ns/pool" -> {namespace, pool}; throws on anything else. */
function parsePool(s) {
  const parts = s.split('/');
  if (parts.length !== 2 || !DNS_LABEL.test(parts[0]) || !DNS_LABEL.test(parts[1])) {
    throw new Error(`invalid pool "${s}": expected <namespace>/<pool>`);
  }
  return { namespace: parts[0], pool: parts[1] };
}

/** "3600", "90s", "5m", "1h" -> seconds; empty -> undefined. */
function parseDuration(s) {
  const v = String(s || '').trim();
  if (!v) return undefined;
  const m = /^(\d+)([smh]?)$/.exec(v);
  if (!m) throw new Error(`invalid duration "${v}": use seconds or a number with s, m or h`);
  return Number(m[1]) * { '': 1, s: 1, m: 60, h: 3600 }[m[2]];
}

/**
 * Socket timeout for each long poll. The server caps a poll's `wait` at 25 s; 40 s leaves room for the
 * network. A POST gets the whole remaining budget instead (see `postTimeoutMs`).
 */
const REQUEST_TIMEOUT_MS = 40000;
const POLL_WAIT = '25s';

/** Fresh ECDSA P-256 key as PEM strings (PKCS#8 private, SPKI public). */
function generateKey() {
  const { privateKey, publicKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
  return {
    privateKey: privateKey.export({ type: 'pkcs8', format: 'pem' }),
    publicKey: publicKey.export({ type: 'spki', format: 'pem' }),
  };
}

/**
 * Request body for POST /v1/allocations. `cache` is the parseCacheInputs result, if any.
 * `auto` is the server default and is left out, so a request without cache inputs
 * stays valid for a controller that predates the cache fields.
 */
function allocationBody({ namespace, pool }, publicKey, ttl, queueTimeout, cache) {
  const b = { namespace, pool, publicKey };
  if (ttl !== undefined) b.ttlSeconds = ttl;
  if (queueTimeout !== undefined) b.queueTimeoutSeconds = queueTimeout;
  if (cache) {
    if (cache.cache !== 'auto') b.cache = cache.cache;
    if (cache.cacheKey) b.cacheKey = cache.cacheKey;
  }
  return b;
}

const CACHE_CHOICES = ['auto', 'none'];
const CACHE_KEY = /^[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9])?$/;

/**
 * Cache inputs (cache.md §5) -> {cache, cacheKey, mode, from}. Empty inputs take
 * the defaults: `auto`, no key (the pool's defaultKey), the pool's mode, no extra refs.
 */
function parseCacheInputs({ cache, cacheKey, cacheMode, cacheFrom }) {
  const c = String(cache || '').trim() || 'auto';
  if (!CACHE_CHOICES.includes(c)) throw new Error(`invalid cache "${c}": use auto or none`);
  const k = String(cacheKey || '').trim();
  if (k && (k.length > 63 || !CACHE_KEY.test(k))) {
    throw new Error(`invalid cache-key "${k}": 1 to 63 of [A-Za-z0-9._-], starting and ending alphanumeric`);
  }
  const m = String(cacheMode || '').trim();
  if (m && m !== 'max' && m !== 'min') throw new Error(`invalid cache-mode "${m}": use max or min`);
  const from = String(cacheFrom || '').split('\n').map((x) => x.trim()).filter(Boolean);
  return { cache: c, cacheKey: k || undefined, mode: m || undefined, from };
}

/** Set `mode=<mode>` in a --cache-to value, replacing any mode the server chose. */
function withMode(to, mode) {
  if (!mode) return to;
  const parts = to.split(',').filter((p) => !p.startsWith('mode='));
  parts.push(`mode=${mode}`);
  return parts.join(',');
}

/**
 * buildx cache flags from the allocation responses and the cache inputs.
 * Returns {to: [...], from: [...], args: [...]}: `to` and `from` hold values for
 * --cache-to and --cache-from without duplicates, `args` the flags themselves.
 */
function cacheFlags(allocs, { mode, from: extra = [] } = {}) {
  const uniq = (xs) => [...new Set(xs)];
  const reg = allocs.map((a) => a && a.cache && a.cache.registry).filter(Boolean);
  const to = uniq(reg.filter((r) => r.to).map((r) => withMode(r.to, mode)));
  const from = uniq([...reg.flatMap((r) => r.from || []), ...extra]);
  const args = [...from.map((f) => `--cache-from=${f}`), ...to.map((t) => `--cache-to=${t}`)];
  return { to, from, args };
}


/**
 * Check the GET /v1/pools answer against the wanted pools (api.md, Clients).
 * Returns the matching pool objects in request order; throws before any allocation.
 */
function validatePools(resp, wanted) {
  const listed = (resp && resp.pools) || [];
  const found = wanted.map((w) => {
    const p = listed.find((x) => x.namespace === w.namespace && x.name === w.pool);
    if (!p) throw new Error(`pool ${w.namespace}/${w.pool} not found or not accessible`);
    if (!p.ready) throw new Error(`pool ${w.namespace}/${w.pool} is not ready`);
    return p;
  });
  if (found.length > 1) {
    if (!resp.perAllocationEndpoints) {
      throw new Error('multiple pools need edge.allocationDomain (perAllocationEndpoints is false)');
    }
    const seen = new Map();
    for (const p of found) {
      if (!p.platforms || p.platforms.length === 0) {
        throw new Error(`pool ${p.namespace}/${p.name} reports no platforms; required with multiple pools`);
      }
      for (const pl of p.platforms) {
        if (seen.has(pl)) {
          throw new Error(`platform ${pl} is reported by both ${seen.get(pl)} and ${p.namespace}/${p.name}`);
        }
        seen.set(pl, `${p.namespace}/${p.name}`);
      }
    }
  }
  return found;
}

/** Argument vectors for `docker buildx create` per node, plus `use`. */
function buildxCommands(builder, nodes) {
  const cmds = nodes.map((n, i) => {
    const args = ['buildx', 'create', '--name', builder];
    if (i > 0) args.push('--append');
    args.push('--node', `n${i}`, '--driver', 'remote');
    if (n.platforms && n.platforms.length) args.push('--platform', n.platforms.join(','));
    const opts = [`cert=${n.certPath}`, `key=${n.keyPath}`, `servername=${n.serverName}`];
    if (n.caPath) opts.unshift(`cacert=${n.caPath}`);
    args.push('--driver-opt', opts.join(','), n.endpoint);
    return args;
  });
  cmds.push(['buildx', 'use', builder]);
  return cmds;
}

/** Allocation ids become path segments and URL parts, so accept only what the server mints. */
function checkAllocationId(id) {
  if (typeof id !== "string" || !/^[a-z0-9]{1,63}$/.test(id)) throw new Error("controller returned an invalid allocation id");
  return id;
}

/**
 * Run the buildx commands. `onCreated` fires once the first `buildx create` succeeds, so the
 * post step never removes a builder this run did not create.
 */
function createBuilder(builder, nodes, run, onCreated) {
  buildxCommands(builder, nodes).forEach((args, i) => {
    run(args);
    if (i === 0) onCreated();
  });
}

/** Write ca/cert/key (0600) for one allocation; returns the paths. */
function writeMaterial(dir, alloc, privateKey) {
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const w = (name, data) => {
    const p = path.join(dir, name);
    fs.writeFileSync(p, data, { mode: 0o600 });
    return p;
  };
  return {
    caPath: alloc.caCert ? w('ca.pem', alloc.caCert) : undefined,
    certPath: w('cert.pem', alloc.clientCert),
    keyPath: w('key.pem', privateKey),
  };
}

/** Minimal HTTP(S) client with an optional pinned CA. Resolves {status, headers, body}. */
function request(url, { method = 'GET', headers = {}, body, ca, timeoutMs = 900000 } = {}) {
  return new Promise((resolve, reject) => {
    const u = new URL(url);
    const lib = u.protocol === 'http:' ? http : https;
    const data = body === undefined ? undefined : Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
    const opts = { method, headers: { ...headers }, timeout: timeoutMs };
    if (ca) opts.ca = ca;
    if (data) opts.headers['Content-Length'] = data.length;
    const req = lib.request(u, opts, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString('utf8') }));
    });
    req.on('timeout', () => req.destroy(new Error(`request to ${u.origin}${u.pathname} timed out`)));
    req.on('error', reject);
    if (data) req.write(data);
    req.end();
  });
}

/** Never put a response body with key material into an error: status and message only. */
function apiError(what, res) {
  let msg = '';
  try {
    const j = JSON.parse(res.body);
    const e = j && j.error;
    msg = e ? [e.code, e.message].filter(Boolean).join(': ') : '';
  } catch { /* not JSON */ }
  return new Error(`${what} failed: HTTP ${res.status}${msg ? ` ${String(msg).slice(0, 200)}` : ''}`);
}

/**
 * The queue timeout a request to `pool` (a GET /v1/pools entry) runs under: the caller's value if
 * set, else the pool default the server reports. Undefined when neither is known (a server that
 * predates `queueTimeoutSeconds` in the pool list).
 */
function effectiveQueueTimeout(queueTimeoutSeconds, pool) {
  if (queueTimeoutSeconds !== undefined) return queueTimeoutSeconds;
  const d = pool && pool.queueTimeoutSeconds;
  return Number.isInteger(d) && d >= 0 ? d : undefined;
}

/**
 * Socket timeout of a POST: the whole budget left. A v3 controller answers within about 20 s, but a
 * v0.1.0 controller holds the POST open through the queue wait and the pod start, so a short timeout
 * would fail an allocation whose id the client never saw.
 */
function postTimeoutMs(remainingMs) {
  return Math.max(1000, remainingMs);
}

/** Bound on the whole retry loop: the request's queue timeout (1 h, the largest allowed, if unknown) + 60 s. It covers the POST, every poll and any fresh POST after a queue timeout. */
function retryBudgetMs(queueTimeoutSeconds) {
  const q = queueTimeoutSeconds === undefined ? 3600 : queueTimeoutSeconds;
  return (q + 60) * 1000;
}

/**
 * Queue timeout for an attempt that starts with `remainingMs` of the retry budget left: the requested
 * value, cut so that a full queue wait still ends inside the budget (budget minus 60 s of slack).
 * Undefined stays undefined: the pool default is unknown, so it is not overridden. Pass the
 * `effectiveQueueTimeout` result so that an unset input still gets a bound from the pool default.
 */
function queueTimeoutFor(queueTimeoutSeconds, remainingMs) {
  if (queueTimeoutSeconds === undefined) return undefined;
  return Math.max(0, Math.min(queueTimeoutSeconds, Math.floor((remainingMs - 60000) / 1000)));
}

/** Total retry budget of a release or a pool listing: neither queues, so neither follows the queue timeout. */
const SHORT_BUDGET_MS = 60000;

// Error codes the controller answers 503 with Retry-After before it claims anything.
const CLAIM_FREE_CODES = new Set(['pool_exhausted', 'start_timeout', 'shutting_down', 'upstream_error']);
// Error codes that end a queued allocation's id for good: GET /v1/allocations/{id} will not change its answer.
const TERMINAL_POLL_CODES = new Set(['pool_exhausted', 'pod_lost', 'not_found', 'forbidden']);

/** True when a 503 body carries one of the controller's claim-free error codes. */
function isClaimFree503(res) {
  try {
    const j = JSON.parse(res.body);
    return CLAIM_FREE_CODES.has(j && j.error && j.error.code);
  } catch { return false; }
}

const MIN_WAIT_MS = 1000;
const MAX_WAIT_MS = 30000;
// Errors raised while connecting, before any byte of the request was written.
const PRE_SEND_CODES = new Set(['ECONNREFUSED', 'ENOTFOUND', 'EAI_AGAIN', 'EHOSTUNREACH', 'ENETUNREACH']);

/** Retry-After header (delta-seconds or HTTP date) -> ms clamped to [1 s, 30 s]; undefined if absent or unusable. */
function retryAfterMs(headers, now = Date.now()) {
  const v = headers && headers['retry-after'];
  if (typeof v !== 'string' || !v.trim()) return undefined;
  let ms;
  if (/^\d+$/.test(v.trim())) ms = Number(v.trim()) * 1000;
  else {
    const t = Date.parse(v);
    if (Number.isNaN(t)) return undefined;
    ms = t - now;
  }
  return Math.min(MAX_WAIT_MS, Math.max(MIN_WAIT_MS, ms));
}

/** True for a failure that happened before the request was sent. A reset or timeout is not. */
function isPreSendError(err) {
  return Boolean(err) && PRE_SEND_CODES.has(err.code);
}

/**
 * `request` with retries that cannot repeat a claim: only HTTP 503 with a usable Retry-After whose
 * body carries a claim-free error code (any 503 with Retry-After when `idempotent`, for GET and
 * DELETE), and connection errors raised before the request was sent. Everything else returns or throws at once.
 * `getToken` runs for every attempt (OIDC tokens are short-lived). Stops retrying when the next wait
 * would pass `budgetMs` from the first attempt, returning the last 503 or throwing the last error.
 * `opts` may be a function of `{remainingMs}` (budget left at the start of the attempt) so a retry can
 * shrink its queue timeout and socket timeout; a retry must not queue for a full timeout again.
 * `what` labels the log line; `log`, `sleep` and `now` are injectable for tests.
 */
async function requestWithRetry(url, opts, { what, getToken, budgetMs, idempotent = false, log = () => {}, sleep = (ms) => new Promise((r) => setTimeout(r, ms)), now = Date.now } = {}) {
  const start = now();
  for (let attempt = 1; ; attempt++) {
    let wait;
    let why;
    try {
      const headers = {};
      if (getToken) headers.Authorization = `Bearer ${await getToken()}`;
      const o = typeof opts === 'function' ? opts({ remainingMs: budgetMs - (now() - start) }) : opts;
      const res = await request(url, { ...o, headers: { ...(o.headers || {}), ...headers } });
      if (res.status !== 503 || !(idempotent || isClaimFree503(res))) return res;
      wait = retryAfterMs(res.headers, now());
      if (wait === undefined) return res;
      why = `HTTP 503`;
      if (now() - start + wait > budgetMs) return res;
    } catch (e) {
      if (!isPreSendError(e)) throw e;
      wait = Math.min(MAX_WAIT_MS, 1000 * 2 ** (attempt - 1));
      why = `${e.code} before send`;
      if (now() - start + wait > budgetMs) throw e;
    }
    log(`${what || 'request'}: ${why}, retrying in ${Math.round(wait / 1000)}s (attempt ${attempt + 1})`);
    await sleep(wait);
  }
}

/** The `error.code` of a response body, or undefined. */
function errorCode(res) {
  try {
    const j = JSON.parse(res.body);
    return (j && j.error && j.error.code) || undefined;
  } catch { return undefined; }
}

/**
 * Long-poll GET /v1/allocations/{id} until the allocation is bound or fails for good. Resolves the
 * last response: 200 (bound) or a terminal answer (a 4xx, or a 503 pool_exhausted or pod_lost).
 * Everything else is transient and polls the same id again after Retry-After (else 2 s): 202, any
 * other 5xx, a 503 upstream_error, and transport failures. Throws once `budgetMs` (from the call)
 * is spent, so it never polls forever. `getToken` runs for every request.
 */
async function pollAllocation(api, id, { ca, getToken, budgetMs, log = () => {}, sleep = (ms) => new Promise((r) => setTimeout(r, ms)), now = Date.now } = {}) {
  const start = now();
  for (;;) {
    const left = budgetMs - (now() - start);
    if (left <= 0) throw new Error(`allocation ${id} was not bound within the queue budget`);
    let wait = 0;
    try {
      const headers = getToken ? { Authorization: `Bearer ${await getToken()}` } : {};
      const res = await request(`${api}/v1/allocations/${id}?wait=${POLL_WAIT}`,
        { ca, headers, timeoutMs: Math.min(REQUEST_TIMEOUT_MS, left + 5000) });
      if (res.status === 200) return res;
      if (res.status === 202) continue;
      if (res.status < 500 || (res.status === 503 && TERMINAL_POLL_CODES.has(errorCode(res)))) return res;
      wait = retryAfterMs(res.headers, now()) ?? 2000;
      log(`poll ${id}: HTTP ${res.status}, polling again in ${Math.round(wait / 1000)}s`);
    } catch (e) {
      wait = 2000;
      log(`poll ${id}: ${e.message}, polling again in 2s`);
    }
    await sleep(wait);
  }
}

/**
 * Allocate one worker. POST /v1/allocations answers 201 (bound) or 202 (queued, with an id); a 202
 * is polled until bound. The queue timeout can run out on a poll (terminal for that id): that starts
 * a fresh POST while the overall budget (`budgetMs`, from the call) allows, as a 503 pool_exhausted
 * from the POST itself does. `attemptOpts({remainingMs})` builds each POST's request options.
 * `onId(id)` fires as soon as an id is known and `onGone(id)` when that id ended for good, so the
 * caller can release exactly the ids that may still hold a worker. Resolves the AllocationResponse.
 */
async function allocate(api, { what, ca, getToken, attemptOpts, budgetMs, onId = () => {}, onGone = () => {}, log = () => {}, sleep, now = Date.now }) {
  const start = now();
  for (;;) {
    const remaining = budgetMs - (now() - start);
    const res = await requestWithRetry(`${api}/v1/allocations`, attemptOpts,
      { what, getToken, budgetMs: remaining, log, sleep, now });
    if (res.status === 201) return JSON.parse(res.body);
    if (res.status !== 202) throw apiError(what, res);
    let id;
    try { id = checkAllocationId(JSON.parse(res.body).id); } catch { throw new Error(`${what} failed: controller returned an invalid allocation id`); }
    onId(id);
    log(`${what}: queued as ${id}`);
    const pr = await pollAllocation(api, id, { ca, getToken, budgetMs: budgetMs - (now() - start), log, sleep, now });
    if (pr.status === 200) return JSON.parse(pr.body);
    onGone(id);
    const wait = retryAfterMs(pr.headers, now()) ?? 5000;
    if (errorCode(pr) !== 'pool_exhausted' || now() - start + wait > budgetMs) throw apiError(what, pr);
    log(`${what}: queue timeout ran out for ${id}, allocating again in ${Math.round(wait / 1000)}s`);
    await (sleep || ((ms) => new Promise((r) => setTimeout(r, ms))))(wait);
  }
}

/** Release allocation `id`. 404 counts as released. Own short retry budget: a down API must not hold the job. */
async function releaseAllocation({ api, ca, getToken, id, log, ...inject }) {
  const res = await requestWithRetry(`${api}/v1/allocations/${id}`, { method: 'DELETE', ca, timeoutMs: 30000 },
    { what: `release ${id}`, getToken, budgetMs: SHORT_BUDGET_MS, idempotent: true, log, ...inject });
  if (res.status !== 204 && res.status !== 404) throw apiError(`release ${id}`, res);
}

/** GitHub OIDC token for `audience`. Masks the bearer and the token via `setSecret`. */
async function fetchOidcToken(audience, setSecret) {
  const base = process.env.ACTIONS_ID_TOKEN_REQUEST_URL;
  const bearer = process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
  if (!base || !bearer) throw new Error('OIDC unavailable: add `permissions: id-token: write` to the job');
  setSecret(bearer);
  const url = `${base}${base.includes('?') ? '&' : '?'}audience=${encodeURIComponent(audience)}`;
  const res = await request(url, { headers: { Authorization: `Bearer ${bearer}` }, timeoutMs: 30000 });
  if (res.status !== 200) throw apiError('OIDC token request', res);
  const token = JSON.parse(res.body).value;
  if (!token) throw new Error('OIDC token response had no value');
  setSecret(token);
  return token;
}

module.exports = {
  parseList, parsePool, REQUEST_TIMEOUT_MS, postTimeoutMs, pollAllocation, allocate, errorCode, parseDuration, generateKey, allocationBody,
  validatePools, buildxCommands, parseCacheInputs, cacheFlags, checkAllocationId, createBuilder, writeMaterial, request, apiError,
  effectiveQueueTimeout, retryBudgetMs, SHORT_BUDGET_MS, isClaimFree503, releaseAllocation, queueTimeoutFor, retryAfterMs, isPreSendError, requestWithRetry, fetchOidcToken,
};
