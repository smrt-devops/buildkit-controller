#!/bin/sh
# Allocate one BuildKit worker from buildkit-controller, expose it as a buildx
# remote builder, run a command, and release on exit.
#
#   TOKEN=<oidc or sa token> API=https://bk.example.com:8443 POOL=ci/amd64 \
#     sh buildkit-allocate.sh sh -c 'docker buildx build --builder "$BUILDER" .'
#
# The builder name is exported to the command as $BUILDER, and the build cache
# flags as $CACHE_ARGS:
#
#   CACHE_KEY=app-main ... sh buildkit-allocate.sh \
#     sh -c 'docker buildx build --builder "$BUILDER" $CACHE_ARGS .'
#
# The controller queues allocations. POST /v1/allocations answers 201 when a worker is bound
# and 202 with an id while it waits in the pool's queue; the script then long-polls
# GET /v1/allocations/{id}?wait=25s until it is bound. All of it is bounded by
# QUEUE_TIMEOUT (the pool default when unset) plus 60 s.
#
# Env: API, POOL (<namespace>/<pool>), TOKEN; optional TTL (seconds),
# QUEUE_TIMEOUT (seconds), API_CA (PEM file trusted for the API listener),
# CACHE_KEY (cache tag, default the pool name) and CACHE (auto or none). A build
# the pool does not trust to write the cache gets --cache-from only.
set -eu

: "${API:?set API}" "${POOL:?set POOL}" "${TOKEN:?set TOKEN}"
[ "$#" -gt 0 ] || { echo "usage: $0 command [args...]" >&2; exit 2; }
NS=${POOL%%/*}; NAME=${POOL#*/}
case $POOL in */*) ;; *) echo "POOL must be <namespace>/<pool>" >&2; exit 2 ;; esac
case $API in https://*) ;; *) echo "API must be https://" >&2; exit 2 ;; esac
case ${CACHE:-auto} in auto|none) ;; *) echo "CACHE must be auto or none" >&2; exit 2 ;; esac
case ${TTL:-} in *[!0-9]*) echo "TTL must be whole seconds, e.g. 3600" >&2; exit 2 ;; esac
case ${QUEUE_TIMEOUT:-} in *[!0-9]*) echo "QUEUE_TIMEOUT must be whole seconds, e.g. 300" >&2; exit 2 ;; esac

D=$(mktemp -d)
chmod 700 "$D"
# Header file keeps the token out of curl's argv (visible in ps); needs curl >= 7.55
# (-H @file); --proto needs 7.52.
( umask 077; printf "Authorization: Bearer %s\n" "$TOKEN" > "$D/auth" )
ID=
BUILDER=
CMD=
CA_OPT=
[ -z "${API_CA:-}" ] || CA_OPT=$API_CA

curl_api() { # curl_api args... ; honours API_CA
  if [ -n "$CA_OPT" ]; then curl -sS --proto =https --cacert "$CA_OPT" "$@"; else curl -sS --proto =https "$@"; fi
}

# idem WHAT BUDGET_SECONDS curl-args... : an idempotent request (GET, DELETE) with auth, a 30 s limit and
# retries of HTTP 503 with Retry-After and of curl failing to resolve or connect (exit 6 or 7), until the
# next wait would pass BUDGET_SECONDS. Leaves the status in $code (000 on a curl failure), the body in $D/io.
idem() {
  _what=$1; _budget=$2; shift 2
  _start=$(date +%s)
  _backoff=1
  while :; do
    _rc=0
    code=$(curl_api --max-time 30 -H @"$D/auth" -D "$D/ih" -o "$D/io" -w '%{http_code}' "$@") || _rc=$?
    [ -n "$code" ] || code=000
    _wait=
    if [ "$code" = 503 ]; then
      _wait=$(retry_wait "$D/ih")
    elif [ "$_rc" = 6 ] || [ "$_rc" = 7 ]; then
      _wait=$_backoff
      _backoff=$(( _backoff * 2 ))
      [ "$_backoff" -le 30 ] || _backoff=30
    fi
    [ -n "$_wait" ] || break
    [ $(( $(date +%s) - _start + _wait )) -le "$_budget" ] || break
    echo "$_what: HTTP $code (curl $_rc), retrying in ${_wait}s" >&2
    sleep "$_wait"
  done
}

# retry_wait HEADERS_FILE -> seconds to wait on stdout: the Retry-After value (integer seconds)
# clamped to 1..30, or nothing when the header is absent or not an integer.
retry_wait() {
  v=$(awk 'tolower($1) == "retry-after:" { gsub(/\r/, "", $2); v = $2 } END { print v }' "$1" 2>/dev/null || :)
  case $v in ''|*[!0-9]*) return 0 ;; esac
  [ "$v" -ge 1 ] || v=1
  [ "$v" -le 30 ] || v=30
  echo "$v"
}

release() {
  rc=$?
  trap - EXIT
  [ -z "$CMD" ] || kill "$CMD" 2>/dev/null || :
  [ -z "$BUILDER" ] || docker buildx rm --force "$BUILDER" >/dev/null 2>&1 || true
  if [ -n "$ID" ]; then
    # The release has its own short budget: a down API must not hold the job for the queue timeout.
    idem "release $ID" 60 -X DELETE "$API/v1/allocations/$ID"
    case $code in 204|404) ;; *) echo "warning: release of $ID returned $code; it drains at expiresAt" >&2 ;; esac
  fi
  rm -rf "$D"
  exit "$rc"
}
trap release EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Without QUEUE_TIMEOUT, the pool default from GET /v1/pools takes its place, so the retries below
# stay inside the default + 60 s. A server that does not report it leaves QUEUE_TIMEOUT unset.
if [ -z "${QUEUE_TIMEOUT:-}" ]; then
  QUEUE_TIMEOUT=
idem "list pools" 60 "$API/v1/pools"
  if [ "$code" = 200 ]; then
    QUEUE_TIMEOUT=$(jq -r --arg ns "$NS" --arg name "$NAME" \
      '.pools[]? | select(.namespace == $ns and .name == $name) | .queueTimeoutSeconds // empty' "$D/io" 2>/dev/null || :)
  fi
  case $QUEUE_TIMEOUT in *[!0-9]*) QUEUE_TIMEOUT= ;; esac
fi
# Total time for retries: queue timeout (1 h, the largest allowed, if unknown) + 60 s.
RETRY_BUDGET=$(( ${QUEUE_TIMEOUT:-3600} + 60 ))

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$D/key.pem" 2>/dev/null
openssl pkey -in "$D/key.pem" -pubout -out "$D/pub.pem"

# A lost response cannot be recovered (the key is ours, the id is not), so only two failures are
# retried, both of which happen before the server claims anything: HTTP 503 with Retry-After, and
# curl failing to resolve or connect (exit 6 or 7). A reset after sending, a timeout and every other
# status end the run. Retries stop once the next wait would pass queue timeout + 60 s.
# The server answers a POST within about 20 s (201, or 202 with an id), so the client timeout is 40 s.
# A 202 is polled by id (poll_alloc). Queue timeout running out on a poll is terminal for that id and
# is retried like a pool_exhausted 503 from the POST: a fresh POST, if the budget allows.
# A retry queues only for what is left of the budget (QUEUE_TIMEOUT less the time already spent), so
# a busy pool does not double the total wait.
# QUEUE_TIMEOUT stays unset only when the server reports no pool default; the server default then applies.
write_body() { # write_body QUEUE_TIMEOUT_OR_EMPTY
jq -n --arg ns "$NS" --arg pool "$NAME" --rawfile pk "$D/pub.pem" \
      --arg ttl "${TTL:-}" --arg qt "$1" \
      --arg ck "${CACHE_KEY:-}" --arg cache "${CACHE:-auto}" \
  '{namespace:$ns,pool:$pool,publicKey:$pk}
   + (if $ttl != "" then {ttlSeconds:($ttl|tonumber)} else {} end)
   + (if $qt != "" then {queueTimeoutSeconds:($qt|tonumber)} else {} end)
   + (if $ck != "" then {cacheKey:$ck} else {} end)
   + (if $cache != "auto" then {cache:$cache} else {} end)' > "$D/req.json"
}
# check_id : validate the id in $D/alloc.json and set ID to it.
check_id() {
  ID=$(jq -r '.id // empty' "$D/alloc.json" 2>/dev/null || :)
  case $ID in
    ''|*[!a-z0-9]*) ID=; echo "controller returned an invalid allocation id" >&2; exit 1 ;;
  esac
  [ "${#ID}" -le 63 ] || { ID=; echo "controller returned an invalid allocation id" >&2; exit 1; }
}

# poll_alloc : long-poll the queued allocation $ID until it is bound or fails for good, leaving the
# status in $code, the headers in $D/hdr and the body in $D/alloc.json. 200 means bound. A terminal
# answer (4xx, or 503 pool_exhausted or pod_lost) clears ID: there is nothing left to release. Anything
# else (202, 5xx, upstream_error, a curl failure) polls the same id again after Retry-After, else 2 s.
# Stops with code 000 once the overall budget is spent, keeping ID so the allocation is released.
poll_alloc() {
  while :; do
    _left=$(( RETRY_BUDGET - ( $(date +%s) - start ) ))
    if [ "$_left" -le 0 ]; then
      echo "allocate $POOL: $ID was not bound within the queue budget" >&2
      code=000
      return 0
    fi
    _mt=$(( _left + 5 ))
    [ "$_mt" -le 40 ] || _mt=40
    _rc=0
    curl_api --max-time "$_mt" -H @"$D/auth" -D "$D/hdr" -o "$D/alloc.json" -w '%{http_code}' \
      "$API/v1/allocations/$ID?wait=25s" > "$D/status" &
    CMD=$!
    wait "$CMD" || _rc=$?
    CMD=
    code=$(cat "$D/status" 2>/dev/null || :)
    [ -n "$code" ] || code=000
    _wait=
    case $code in
      200) return 0 ;;
      202) continue ;;
      000) ;;
      503)
        case $(jq -r '.error.code // empty' "$D/alloc.json" 2>/dev/null || :) in
          pool_exhausted|pod_lost) ID=; return 0 ;;
        esac
        _wait=$(retry_wait "$D/hdr") ;;
      4??) ID=; return 0 ;;
      *) _wait=$(retry_wait "$D/hdr") ;;
    esac
    [ -n "$_wait" ] || _wait=2
    echo "poll $ID: HTTP $code (curl $_rc), polling again in ${_wait}s" >&2
    sleep "$_wait"
  done
}

start=$(date +%s)
attempt=1
backoff=1
while :; do
  qt=${QUEUE_TIMEOUT:-}
  if [ -n "$qt" ]; then
    qt=$(( QUEUE_TIMEOUT - ( $(date +%s) - start ) ))
    [ "$qt" -ge 0 ] || qt=0
    [ "$qt" -le "$QUEUE_TIMEOUT" ] || qt=$QUEUE_TIMEOUT
  fi
  write_body "$qt"
  rc=0
  # In the background, so a TERM runs the release trap at once instead of after the request.
  curl_api --max-time 40 -H @"$D/auth" -D "$D/hdr" -o "$D/alloc.json" -w '%{http_code}' \
    -H 'Content-Type: application/json' --data-binary @"$D/req.json" "$API/v1/allocations" > "$D/status" &
  CMD=$!
  wait "$CMD" || rc=$?
  CMD=
  code=$(cat "$D/status" 2>/dev/null || :)
  wait=
  if [ "$code" = 202 ]; then
    check_id
    echo "allocate $POOL: queued as $ID" >&2
    poll_alloc
  fi
  if [ "$code" = 503 ]; then
    # Only the codes the controller sends with Retry-After before it claims a worker; an intermediary
    # that adds Retry-After to its own 503 may have forwarded the request.
    case $(jq -r '.error.code // empty' "$D/alloc.json" 2>/dev/null || :) in
      pool_exhausted|start_timeout|upstream_error) wait=$(retry_wait "$D/hdr") ;;
    esac
    why="HTTP 503"
  elif [ "$rc" = 6 ] || [ "$rc" = 7 ]; then
    wait=$backoff
    backoff=$(( backoff * 2 ))
    [ "$backoff" -le 30 ] || backoff=30
    why="connect failed (curl $rc)"
  fi
  [ -n "$wait" ] || break
  [ $(( $(date +%s) - start + wait )) -le "$RETRY_BUDGET" ] || break
  attempt=$(( attempt + 1 ))
  echo "allocate $POOL: $why, retrying in ${wait}s (attempt $attempt)" >&2
  sleep "$wait"
done
case $code in
  200|201) ;;
  *)
    detail=$(jq -r '.error // empty | "\(.code): \(.message)"' "$D/alloc.json" 2>/dev/null || :)
    echo "allocation failed: HTTP ${code:-000} $detail" >&2
    exit 1 ;;
esac

check_id
BUILDER="bk-$ID"
jq -r .clientCert "$D/alloc.json" > "$D/cert.pem"
jq -r '.caCert // empty' "$D/alloc.json" > "$D/ca.pem"
OPTS="cert=$D/cert.pem,key=$D/key.pem,servername=$(jq -r .serverName "$D/alloc.json")"
[ ! -s "$D/ca.pem" ] || OPTS="cacert=$D/ca.pem,$OPTS"

docker buildx create --name "$BUILDER" --driver remote --driver-opt "$OPTS" \
  "$(jq -r .endpoint "$D/alloc.json")" >/dev/null
# Cache values hold no spaces, so an unquoted $CACHE_ARGS splits into one word per flag.
CACHE_ARGS=$(jq -r '[(.cache.registry.from // [])[] | "--cache-from=\(.)"]
  + [.cache.registry.to // empty | "--cache-to=\(.)"] | join(" ")' "$D/alloc.json")
[ -z "$CACHE_ARGS" ] || echo "cache flags: $CACHE_ARGS" >&2
export BUILDER CACHE_ARGS
# The command runs in the background with the original stdin, so a TERM reaches the release trap
# at once, which stops the command and releases the worker.
exec 3<&0
"$@" <&3 3<&- &
CMD=$!
wait "$CMD" || { rc=$?; CMD=; exit "$rc"; }
CMD=
