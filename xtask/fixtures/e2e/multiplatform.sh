#!/usr/bin/env bash
# Builds a two-node buildx builder the way the GitHub Action does, with one
# allocation per pool, `buildx create` for the first and `--append --node n<i>
# --platform <pool platforms>` for the rest. Then it builds for both platforms.
# Env: API API_CA TOKEN POOLS ("ns/pool=platform ...")
set -euo pipefail
D=$(mktemp -d); chmod 700 "$D"
IDS=(); NAME=
cleanup() {
  [ -z "$NAME" ] || docker buildx rm --force "$NAME" >/dev/null 2>&1 || true
  for id in "${IDS[@]:-}"; do
    [ -z "$id" ] || curl -sS --cacert "$API_CA" -X DELETE -H "Authorization: Bearer $TOKEN" "$API/v1/allocations/$id" -o /dev/null || true
  done
  rm -rf "$D"
}
trap cleanup EXIT
i=0
for spec in $POOLS; do
  pool=${spec%%=*}; plat=${spec#*=}; ns=${pool%%/*}; name=${pool#*/}
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$D/k$i.pem" 2>/dev/null
  openssl pkey -in "$D/k$i.pem" -pubout -out "$D/p$i.pem"
  jq -n --arg ns "$ns" --arg pool "$name" --rawfile pk "$D/p$i.pem" '{namespace:$ns,pool:$pool,publicKey:$pk,queueTimeoutSeconds:240}' |
    curl -sS --fail-with-body --max-time 900 --cacert "$API_CA" -H "Authorization: Bearer $TOKEN" \
      -H 'Content-Type: application/json' --data-binary @- "$API/v1/allocations" > "$D/a$i.json"
  IDS+=("$(jq -r .id "$D/a$i.json")")
  jq -r .clientCert "$D/a$i.json" > "$D/c$i.pem"
  jq -r '.caCert // empty' "$D/a$i.json" > "$D/ca$i.pem"
  opts="cacert=$D/ca$i.pem,cert=$D/c$i.pem,key=$D/k$i.pem,servername=$(jq -r .serverName "$D/a$i.json")"
  ep=$(jq -r .endpoint "$D/a$i.json")
  if [ "$i" = 0 ]; then
    NAME="bk-${IDS[0]}"
    docker buildx create --name "$NAME" --node n0 --driver remote --platform "$plat" --driver-opt "$opts" "$ep" >/dev/null
  else
    docker buildx create --append --name "$NAME" --node "n$i" --platform "$plat" --driver-opt "$opts" "$ep" >/dev/null
  fi
  i=$((i+1))
done
docker buildx inspect --builder "$NAME" --bootstrap
ALL=$(echo "$POOLS" | tr ' ' '\n' | sed "s/.*=//; s/,.*//" | paste -sd, -)
# Emulated RUN needs binfmt handlers in the kernel that runs the workers. CI
# installs them from a digest-pinned image. Locally the script uses what is
# already there. The digest was resolved with:
#   docker buildx imagetools inspect tonistiigi/binfmt:qemu-v9.2.2-52
BINFMT=tonistiigi/binfmt@sha256:1b804311fe87047a4c96d38b4b3ef6f62fca8cd125265917a9e3dc3c996c39e6
if [ "${CI:-}" = true ]; then
  docker run --rm --privileged "$BINFMT" --install all >/dev/null
fi
HOST=$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')
FOREIGN=$(echo "$ALL" | tr , '\n' | grep -v "$HOST" | head -1)
if docker run --rm --platform "linux/${FOREIGN#linux/}" alpine:3.20 true >/dev/null 2>&1; then
  mkdir "$D/ctx"
  printf 'FROM alpine:3.20\nRUN uname -m > /arch\n' > "$D/ctx/Dockerfile"
  CTX=$D/ctx
  echo "multiplatform path: RUN step, emulated in $FOREIGN"
elif [ "${CI:-}" = true ]; then
  echo "CI: no emulation for $FOREIGN after the binfmt install, refusing the COPY-only path" >&2
  exit 1
else
  echo "multiplatform path: COPY-only, no emulation for $FOREIGN"
fi
docker buildx build --builder "$NAME" --progress=plain --platform "$ALL" -o type=cacheonly "$CTX"
# Each node builds its own platform on its own.
for spec in $POOLS; do
  docker buildx build --builder "$NAME" --progress=plain --platform "$(echo "${spec#*=}" | sed "s/,.*//")" -o type=cacheonly "$CTX"
done
