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
  "watchNamespace" .Values.watchNamespace
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
  "shutdown" .Values.shutdown
  "claims" (dict "enabled" .Values.claims.enabled "namespaces" .Values.claims.namespaces) -}}
{{- toYaml $cfg -}}
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
