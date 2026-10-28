#!/bin/sh
# Soak client for `cargo xtask soak`. It makes one fresh allocation per build,
# through the API (ServiceAccount token) and the edge, until /work/stop exists.
# It writes one line per build to /work/log:
#   <n> <start> <end> ok|fail:<stage> retries=<r> [allocated=<201 time>] [lastpost=<last POST attempt, fail:post only>] [<detail>]
# Retries follow the documented client contract. The client retries only failures before the request was
# sent (connect, or TLS handshake, where it re-reads trust from the bundle
# ConfigMap) and 503 with Retry-After.
API=https://buildkit-controller.buildkit-system.svc:8443
CM=https://kubernetes.default.svc/api/v1/namespaces/buildkit-system/configmaps/buildkit-controller-ca-bundle
SA=/var/run/secrets/kubernetes.io/serviceaccount
R=/work/run
mkdir -p "$R"
# The client key pair, made by the init container (the curl image has no openssl).
# Only the public half is sent; /work/pub.json is it as JSON string content.
if [ ! -s /work/key.pem ] || [ ! -s /work/pub.json ]; then
  echo "no client key pair in /work" >&2
  exit 1
fi

# String field $1 of the JSON in file $2, with JSON escapes for \n undone.
field() {
  v=$(tr -d '\n' < "$2" | sed -n "s/.*\"$1\": *\"\([^\"]*\)\".*/\1/p")
  printf '%b' "$v"
}

# Trust for the API and the edge. It is the bundle ConfigMap (self-issued serving
# cert) plus the CA of the user serving cert (scenario b). Kept on failure.
trust() {
  if curl -sf --max-time 5 --cacert "$SA/ca.crt" -H "Authorization: Bearer $(cat $SA/token)" \
      "$CM" -o "$R/cm.json"; then
    field 'ca.crt' "$R/cm.json" > "$R/trust.new"
    cat /user-ca/ca.crt >> "$R/trust.new"
    mv "$R/trust.new" "$R/trust.pem"
  fi
}

once() {
  retries=0
  trust
  while :; do
    lp=$(date +%s)  # last POST attempt
    code=$(curl -s --max-time 200 --cacert "$R/trust.pem" -o "$R/resp.json" -D "$R/hdr" \
      -w '%{http_code}' -H "Authorization: Bearer $(cat /var/run/secrets/bk/token)" \
      -H 'Content-Type: application/json' \
      -d '{"namespace":"bk-soak","pool":"soak","ttlSeconds":60,"publicKey":"'"$(cat /work/pub.json)"'"}' \
      "$API/v1/allocations")
    rc=$?
    case "$rc" in
      6|7|35|60) ;;            # DNS, connect, TLS handshake and verify failures mean nothing was sent
      0) [ "$code" = 201 ] && break
         ra=$(tr -d '\r' < "$R/hdr" | sed -n 's/^[Rr]etry-[Aa]fter: *//p')
         if [ "$code" != 503 ] || [ -z "$ra" ]; then
           echo "fail:post retries=$retries lastpost=$lp http=$code $(tr -d '\n' < "$R/resp.json" | head -c 200)"; return 1
         fi
         sleep "$ra" ;;
      *) echo "fail:post retries=$retries lastpost=$lp curl=$rc"; return 1 ;;
    esac
    retries=$((retries + 1))
    [ "$retries" -ge 60 ] && { echo "fail:post retries=$retries lastpost=$lp rc=$rc http=$code"; return 1; }
    sleep 1
    trust
  done
  got=$(date +%s)  # the 201 arrived, so the allocation exists from here
  id=$(field id "$R/resp.json")
  field clientCert "$R/resp.json" > "$R/cert.pem"
  cp /work/key.pem "$R/key.pem"
  { field caCert "$R/resp.json"; cat "$R/trust.pem"; } > "$R/ca.pem"
  ep=$(field endpoint "$R/resp.json")
  sn=$(field serverName "$R/resp.json")
  if /tools/buildctl --addr "$ep" --tlsservername "$sn" --tlscacert "$R/ca.pem" \
      --tlscert "$R/cert.pem" --tlskey "$R/key.pem" build --progress=plain --no-cache \
      --frontend dockerfile.v0 --local context=/work/small --local dockerfile=/work/small \
      > "$R/build.out" 2>&1 && grep -q built-through-edge "$R/build.out"; then
    res="ok retries=$retries allocated=$got alloc=$id"
  else
    cp "$R/build.out" "/work/fail.$n"
    res="fail:build retries=$retries allocated=$got alloc=$id $(grep -v '^#' "$R/build.out" | tail -1 | head -c 200)"
  fi
  dc=$(curl -s --max-time 30 --cacert "$R/trust.pem" -o /dev/null -w '%{http_code}' -X DELETE \
    -H "Authorization: Bearer $(cat /var/run/secrets/bk/token)" "$API/v1/allocations/$id")
  echo "$res release=$dc"
  case "$res" in ok*) return 0 ;; *) return 1 ;; esac
}

n=0
while [ ! -f /work/stop ]; do
  n=$((n + 1))
  s=$(date +%s)
  r=$(once)
  echo "$n $s $(date +%s) $r" >> /work/log
done
touch /work/stopped
# Stay up so the harness can read the log.
exec sleep infinity
