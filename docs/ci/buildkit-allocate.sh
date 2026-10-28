#!/bin/sh
# Allocate one BuildKit worker from buildkit-controller, expose it as a buildx
# remote builder, run a command, and release on exit.
#
#   TOKEN=<oidc or sa token> API=https://bk.example.com:8443 POOL=ci/amd64 \
#     sh buildkit-allocate.sh sh -c 'docker buildx build --builder "$BUILDER" .'
#
# The builder name is exported to the command as $BUILDER, and the registry cache
# flags as $CACHE_ARGS:
#
#   CACHE_KEY=app-main ... sh buildkit-allocate.sh \
#     sh -c 'docker buildx build --builder "$BUILDER" $CACHE_ARGS .'
#
# Env: API, POOL (<namespace>/<pool>), TOKEN; optional TTL (seconds),
# QUEUE_TIMEOUT (seconds), API_CA (PEM file trusted for the API listener),
# CACHE_KEY (cache key of both layers) and CACHE (auto, registry, local or none).
set -eu

: "${API:?set API}" "${POOL:?set POOL}" "${TOKEN:?set TOKEN}"
[ "$#" -gt 0 ] || { echo "usage: $0 command [args...]" >&2; exit 2; }
NS=${POOL%%/*}; NAME=${POOL#*/}
case $POOL in */*) ;; *) echo "POOL must be <namespace>/<pool>" >&2; exit 2 ;; esac
case $API in https://*) ;; *) echo "API must be https://" >&2; exit 2 ;; esac
case ${CACHE:-auto} in auto|registry|local|none) ;; *) echo "CACHE must be auto, registry, local or none" >&2; exit 2 ;; esac

D=$(mktemp -d)
chmod 700 "$D"
# Header file keeps the token out of curl's argv (visible in ps); needs curl >= 7.55
# (-H @file); --proto needs 7.52.
( umask 077; printf "Authorization: Bearer %s\n" "$TOKEN" > "$D/auth" )
ID=
BUILDER=
CA_OPT=
[ -z "${API_CA:-}" ] || CA_OPT=$API_CA

curl_api() { # curl_api args... ; honours API_CA
  if [ -n "$CA_OPT" ]; then curl -sS --proto =https --cacert "$CA_OPT" "$@"; else curl -sS --proto =https "$@"; fi
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
  [ -z "$BUILDER" ] || docker buildx rm --force "$BUILDER" >/dev/null 2>&1 || true
  if [ -n "$ID" ]; then
    # DELETE is idempotent, so a 503 with Retry-After is retried within the same time bound.
    start=$(date +%s)
    while :; do
      code=$(curl_api -D "$D/del.hdr" -o /dev/null -w '%{http_code}' -X DELETE \
        -H @"$D/auth" "$API/v1/allocations/$ID" || echo 000)
      [ "$code" = 503 ] || break
      wait=$(retry_wait "$D/del.hdr")
      [ -n "$wait" ] || break
      [ $(( $(date +%s) - start + wait )) -le "$RETRY_BUDGET" ] || break
      echo "release $ID: HTTP 503, retrying in ${wait}s" >&2
      sleep "$wait"
    done
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
  QUEUE_TIMEOUT=$(curl_api --max-time 30 -H @"$D/auth" "$API/v1/pools" 2>/dev/null |
    jq -r --arg ns "$NS" --arg name "$NAME" \
      '.pools[]? | select(.namespace == $ns and .name == $name) | .queueTimeoutSeconds // empty' 2>/dev/null || :)
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
# The client timeout covers queue timeout + startTimeout ceiling (1800) + 10 s.
# A retry queues only for what is left of the budget (QUEUE_TIMEOUT less the time already spent), so
# a busy pool, which answers 503 after the full queue timeout, does not double the total wait.
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
  MAX_TIME=$(( ${qt:-3600} + 1800 + 10 ))
  rc=0
  curl_api --max-time "$MAX_TIME" -H @"$D/auth" -D "$D/hdr" -o "$D/alloc.json" -w '%{http_code}' \
    -H 'Content-Type: application/json' --data-binary @"$D/req.json" "$API/v1/allocations" > "$D/status" || rc=$?
  code=$(cat "$D/status" 2>/dev/null || :)
  wait=
  if [ "$code" = 503 ]; then
    wait=$(retry_wait "$D/hdr")
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
if [ "$code" != 201 ]; then
  detail=$(jq -r '.error // empty | "\(.code): \(.message)"' "$D/alloc.json" 2>/dev/null || :)
  echo "allocation failed: HTTP ${code:-000} $detail" >&2
  exit 1
fi

ID=$(jq -r .id "$D/alloc.json")
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
CACHE_LOCAL=$(jq -r '.cache.local.result // empty' "$D/alloc.json")
[ -z "$CACHE_LOCAL" ] || echo "cache: key $(jq -r .cache.key "$D/alloc.json"), local $CACHE_LOCAL" >&2
[ -z "$CACHE_ARGS" ] || echo "cache flags: $CACHE_ARGS" >&2
export BUILDER CACHE_ARGS
"$@"
