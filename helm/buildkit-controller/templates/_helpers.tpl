{{- define "bkc.name" -}}
{{- default "buildkit-controller" .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "bkc.fullname" -}}
{{- if .Values.fullnameOverride -}}{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}{{- include "bkc.name" . -}}{{- end -}}
{{- end -}}

{{/* app.kubernetes.io/name is fixed: worker NetworkPolicies admit pods with this label. */}}
{{- define "bkc.selectorLabels" -}}
app.kubernetes.io/name: buildkit-controller
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "bkc.labels" -}}
{{ include "bkc.selectorLabels" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{- define "bkc.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}

{{/* The config file, built from values. Mounted CA bundles get a fixed path. */}}
{{- define "bkc.config" -}}
{{- $issuers := list -}}
{{- range $i, $iss := .Values.auth.oidc -}}
  {{- $c := omit $iss "caBundleConfigMap" -}}
  {{- if $iss.caBundleConfigMap -}}
    {{- $_ := set $c "caBundleFile" (printf "/etc/buildkit-controller/oidc-ca/%d/%s" $i (default "ca.crt" $iss.caBundleConfigMap.key)) -}}
  {{- end -}}
  {{- $issuers = append $issuers $c -}}
{{- end -}}
{{- $fn := include "bkc.fullname" . -}}
{{- $cfg := dict
  "listen" (dict "api" "0.0.0.0:8443" "edge" "0.0.0.0:8444" "ops" (printf "0.0.0.0:%d" (int .Values.metrics.port)))
  "pki" (dict "caSecretName" (printf "%s-ca" $fn))
  "leaderElection" (dict "leaseName" (printf "%s-leader" $fn))
  "edge" (dict
    "endpoint" (required "edge.endpoint is required" .Values.edge.endpoint)
    "serverName" (required "edge.serverName is required" .Values.edge.serverName)
    "allocationDomain" .Values.edge.allocationDomain
    "extraNames" .Values.edge.extraNames
    "serviceName" $fn
    "tls" (dict "secretName" .Values.edge.tls.secretName)
    "handshakeTimeout" .Values.edge.handshakeTimeout
    "maxConnections" (int .Values.edge.maxConnections)
    "maxHandshakes" (int .Values.edge.maxHandshakes)
    "maxHandshakesPerIp" (int .Values.edge.maxHandshakesPerIp))
  "api" (dict
    "maxConnections" (int .Values.api.maxConnections)
    "maxConnectionsPerIp" (int .Values.api.maxConnectionsPerIp))
  "auth" (dict "oidc" $issuers "serviceAccount" .Values.auth.serviceAccount)
  "workerNetwork" .Values.workerNetwork
  "shutdown" (ternary .Values.shutdown (omit .Values.shutdown "drainTimeout") (not (empty .Values.shutdown.drainTimeout)))
  "claims" (dict "namespaces" .Values.claims.namespaces) -}}
{{- if has .Release.Namespace (default (list) .Values.claims.namespaces) -}}
{{- fail (printf "claims.namespaces must not include the release namespace %s: the API's claims live there and the leader trusts their owner and cache fields, so only cluster admins may create claims there" .Release.Namespace) -}}
{{- end -}}
{{- if .Values.cache.enabled -}}
  {{- $_ := set $cfg "registry" (dict "host" (include "bkc.registryHost" .) "tlsSecretName" (printf "%s-registry-tls" $fn)) -}}
{{- end -}}
{{- /* PROXY protocol only when on, so configs without it (and their checksum) stay the same. */ -}}
{{- range $l := list "api" "edge" -}}
  {{- $p := (get $.Values $l).proxyProtocol -}}
  {{- if $p.enabled -}}
    {{- if not $p.trustedCIDRs }}{{ fail (printf "%s.proxyProtocol.trustedCIDRs is required when %s.proxyProtocol.enabled is true: list the gateway's source addresses" $l $l) }}{{ end -}}
    {{- $_ := set (get $cfg $l) "proxyProtocol" (dict "enabled" true "trustedCIDRs" $p.trustedCIDRs "headerTimeout" $p.headerTimeout) -}}
  {{- end -}}
{{- end -}}
{{- /* Scoped: the list. Cluster-wide: no key. */ -}}
{{- with include "bkc.watchNamespaces" . | fromJsonArray -}}
  {{- $_ := set $cfg "watchNamespaces" . -}}
{{- end -}}
{{- toYaml $cfg -}}
{{- end -}}

{{/* The cache registry Service, as workers reach it. */}}
{{- define "bkc.registryHost" -}}
{{- printf "%s-registry.%s.svc:5000" (include "bkc.fullname" .) .Release.Namespace -}}
{{- end -}}

{{/* Labels of the cache registry pod. app.kubernetes.io/name is fixed: worker NetworkPolicies admit it. */}}
{{- define "bkc.registrySelectorLabels" -}}
app.kubernetes.io/name: buildkit-registry
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Total seconds of "1h2m3s"-style durations. */}}
{{- define "bkc.seconds" -}}
{{- $t := 0 -}}
{{- range $m := regexFindAll "[0-9]+[hms]" (toString .) -1 -}}
  {{- $n := int (trimSuffix "h" (trimSuffix "m" (trimSuffix "s" $m))) -}}
  {{- if hasSuffix "h" $m -}}{{- $t = add $t (mul $n 3600) -}}
  {{- else if hasSuffix "m" $m -}}{{- $t = add $t (mul $n 60) -}}
  {{- else -}}{{- $t = add $t $n -}}{{- end -}}
{{- end -}}
{{- $t -}}
{{- end -}}

{{/* Watched namespaces as a JSON list, [] = cluster-wide. */}}
{{- define "bkc.watchNamespaces" -}}
{{- $list := default (list) .Values.watchNamespaces -}}
{{- if has .Release.Namespace $list -}}
{{- fail (printf "watchNamespaces must not include the release namespace %s: it holds the CA Secret, and pool workloads must not run beside it" .Release.Namespace) -}}
{{- end -}}
{{- $list | toJson -}}
{{- end -}}

{{/* Seconds the controller may drain: shutdown.drainTimeout, or when unset the
longest edge connection lifetime (24h maxTTL cap + 5m cert margin = 86700 s). */}}
{{- define "bkc.drainSeconds" -}}
{{- if .Values.shutdown.drainTimeout -}}
{{- include "bkc.seconds" .Values.shutdown.drainTimeout -}}
{{- else -}}
86700
{{- end -}}
{{- end -}}
