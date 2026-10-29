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
# Env: API, POOL (<namespace>/<pool>), TOKEN or TOKEN_FILE; optional TTL (seconds),
# QUEUE_TIMEOUT (seconds), API_CA (PEM file trusted for the API listener),
# CACHE_KEY (cache tag, default the pool name) and CACHE (auto or none). A build
# the pool does not trust to write the cache gets --cache-from only.
#
# TOKEN is read once and unset, so the command does not inherit it. TOKEN_FILE names a file
# that holds the token and is read again before every request, including the release at the
# end: use it when the token is short-lived (a CI OIDC token, kubectl create token) and the
# build outlasts it. The token must be valid when the release runs.
set -eu

: "${API:?set API}" "${POOL:?set POOL}"
[ -n "${TOKEN:-}" ] || [ -n "${TOKEN_FILE:-}" ] || { echo "set TOKEN or TOKEN_FILE" >&2; exit 2; }
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
# write_auth : with TOKEN_FILE, rewrite $D/auth from the file. An empty or unreadable file keeps the
# last header.
write_auth() {
  [ -n "${TOKEN_FILE:-}" ] || return 0
  _t=$(tr -d '\r\n' < "$TOKEN_FILE" 2>/dev/null || :)
  [ -z "$_t" ] || ( umask 077; printf "Authorization: Bearer %s\n" "$_t" > "$D/auth.new" && mv "$D/auth.new" "$D/auth" )
}
if [ -n "${TOKEN_FILE:-}" ]; then
  write_auth
  [ -s "$D/auth" ] || { echo "TOKEN_FILE is empty or unreadable" >&2; rm -rf "$D"; exit 2; }
else
  ( umask 077; printf "Authorization: Bearer %s\n" "$TOKEN" > "$D/auth" )
fi
unset TOKEN
ID=
BUILDER=
CMD=
CA_OPT=
[ -z "${API_CA:-}" ] || CA_OPT=$API_CA

curl_api() { # curl_api args... ; honours API_CA, re-reads TOKEN_FILE
  write_auth
  if [ -n "$CA_OPT" ]; then curl -sS --proto =https --cacert "$CA_OPT" "$@"; else curl -sS --proto =https "$@"; fi
}

# idem WHAT BUDGET_SECONDS curl-args... : a request (GET, DELETE) with auth, a 30 s limit and retries
# of any curl failure and any 5xx, after Retry-After when the answer has one, else after 1, 2, 4 ... 30 s,
# until the next wait would pass BUDGET_SECONDS. Leaves the status in $code (000 on a curl failure),
# the body in $D/io.
idem() {
  _what=$1; _budget=$2; shift 2
  _start=$(date +%s)
  _backoff=1
  while :; do
    _rc=0
    code=$(curl_api --max-time 30 -H @"$D/auth" -D "$D/ih" -o "$D/io" -w '%{http_code}' "$@") || _rc=$?
    [ -n "$code" ] || code=000
    case $code in 000|5??) ;; *) break ;; esac
    _wait=
    [ "$code" = 000 ] || _wait=$(retry_wait "$D/ih")
    if [ -z "$_wait" ]; then
      _wait=$_backoff
      _backoff=$(( _backoff * 2 ))
      [ "$_backoff" -le 30 ] || _backoff=30
    fi
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
# A pool entry with queueTimeoutSeconds marks the current server (CURRENT=1), which answers a POST
# within about 20 s: its POST gets a 40 s limit. v0.1.0 holds the POST open through the queue wait, so
# without the field the POST keeps the whole budget (upgrade shim; drop it in the release after v3).
POOL_QT=
idem "list pools" 60 "$API/v1/pools"
if [ "$code" = 200 ]; then
  POOL_QT=$(jq -r --arg ns "$NS" --arg name "$NAME" \
    '.pools[]? | select(.namespace == $ns and .name == $name) | .queueTimeoutSeconds // empty' "$D/io" 2>/dev/null || :)
fi
case $POOL_QT in *[!0-9]*) POOL_QT= ;; esac
CURRENT=
[ -z "$POOL_QT" ] || CURRENT=1
[ -n "${QUEUE_TIMEOUT:-}" ] || QUEUE_TIMEOUT=$POOL_QT
# Total time for retries: queue timeout (1 h, the largest allowed, if unknown) + 60 s.
RETRY_BUDGET=$(( ${QUEUE_TIMEOUT:-3600} + 60 ))

# new_key : a fresh key pair. Only the public half is sent, and it names the allocation.
new_key() {
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$D/key.pem" 2>/dev/null
  openssl pkey -in "$D/key.pem" -pubout -out "$D/pub.pem"
}
new_key

# The public key names the allocation, so a POST is safe to repeat: the controller answers a repeat
# with the same key for the allocation the first one made, and never makes a second. Any curl failure
# (a reset or a timeout after sending too) and any 5xx is retried with the same key, after Retry-After
# when the answer has one, else after 1, 2, 4 ... 30 s. Retries stop once the next wait would pass
# queue timeout + 60 s. A 202 is polled by id (poll_alloc). The queue timeout running out (503
# pool_exhausted, on a poll or on the POST) ends that allocation: a fresh POST with a new key follows,
# if the budget allows. 503 pod_lost and every 4xx end the run.
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
    curl_api --max-time "$_mt" -H @"$D/auth" -D "$D/hdr" -o "$D/alloc.json" -w '%{http_code} %{time_total}' \
      "$API/v1/allocations/$ID?wait=25s" > "$D/status" &
    CMD=$!
    wait "$CMD" || _rc=$?
    CMD=
    _tt=
    read -r code _tt < "$D/status" 2>/dev/null || :
    [ -n "$code" ] || code=000
    _wait=
    case $code in
      200) return 0 ;;
      202)
        # A poll answered in under 1 s was not held for its wait: pause 1-2 s before asking again.
        case $_tt in 0[.,]*) sleep $(( 1 + $(od -An -N1 -tu1 /dev/urandom) % 2 )) ;; esac
        continue ;;
      000) ;;
      503)
        case $(jq -r '.error.code // empty' "$D/alloc.json" 2>/dev/null || :) in
          pool_exhausted|pod_lost) ID=; return 0 ;;
        esac
        _wait=$(retry_wait "$D/hdr") ;;
      401) return 0 ;; # may be the token, not the allocation: keep ID for the release
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
  _pt=$(( RETRY_BUDGET - ( $(date +%s) - start ) ))
  [ "$_pt" -ge 1 ] || _pt=1
  [ -z "$CURRENT" ] || [ "$_pt" -le 40 ] || _pt=40
  curl_api --max-time "$_pt" -H @"$D/auth" -D "$D/hdr" -o "$D/alloc.json" -w '%{http_code}' \
    -H 'Content-Type: application/json' --data-binary @"$D/req.json" "$API/v1/allocations" > "$D/status" &
  CMD=$!
  wait "$CMD" || rc=$?
  CMD=
  code=$(cat "$D/status" 2>/dev/null || :)
  [ -n "$code" ] || code=000
  if [ "$code" = 202 ]; then
    check_id
    echo "allocate $POOL: queued as $ID" >&2
    poll_alloc
    # 000 here is the budget running out while polling, not a failed POST.
    [ "$code" != 000 ] || break
  fi
  wait=
  fresh=
  case $code in
    000|5??)
      err=
      [ "$code" = 000 ] || err=$(jq -r '.error.code // empty' "$D/alloc.json" 2>/dev/null || :)
      case $err in
        pod_lost) break ;;
        pool_exhausted)
          wait=$(retry_wait "$D/hdr")
          [ -n "$wait" ] || wait=5
          fresh=1 ;;
        *)
          [ "$code" = 000 ] || wait=$(retry_wait "$D/hdr")
          if [ -z "$wait" ]; then
            wait=$backoff
            backoff=$(( backoff * 2 ))
            [ "$backoff" -le 30 ] || backoff=30
          fi ;;
      esac ;;
    *) break ;;
  esac
  [ $(( $(date +%s) - start + wait )) -le "$RETRY_BUDGET" ] || break
  # A fresh POST would carry queue timeout 0 and fail at once: end with the last answer instead.
  [ -z "$fresh" ] || [ "${QUEUE_TIMEOUT:-0}" -eq 0 ] || [ $(( $(date +%s) - start + wait )) -lt "$QUEUE_TIMEOUT" ] || break
  attempt=$(( attempt + 1 ))
  if [ -n "$fresh" ]; then
    echo "allocate $POOL: queue timeout ran out, allocating again with a new key in ${wait}s (attempt $attempt)" >&2
    new_key
    backoff=1
  else
    echo "allocate $POOL: HTTP $code (curl $rc), retrying with the same key in ${wait}s (attempt $attempt)" >&2
  fi
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
