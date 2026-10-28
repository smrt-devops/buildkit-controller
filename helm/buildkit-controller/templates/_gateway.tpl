{{/*
What the chart renders for Gateway API exposure, as JSON:
  apiRefs, edgeRefs   parentRefs per route; [] = that route is not rendered
  apiHostnames        HTTPRoute or TLSRoute hostnames of the API
  edgeHostnames       TLSRoute hostnames (SNI) of the edge
  missing             kinds the routes need that the cluster does not serve
  envoyMissing        gateway.envoy.enabled but no BackendTrafficPolicy CRD
Fails when a route has no parentRefs and service.type is not LoadBalancer.
*/}}
{{- define "bkc.gateway" -}}
{{- $gw := .Values.gateway -}}
{{- $out := dict "apiRefs" (list) "edgeRefs" (list) "apiHostnames" (list) "edgeHostnames" (list) "missing" (list) "envoyMissing" false -}}
{{- if $gw.enabled -}}
  {{- $apiRefs := default $gw.parentRefs $gw.api.parentRefs -}}
  {{- $edgeRefs := default $gw.parentRefs $gw.edge.parentRefs -}}
  {{- $empty := list -}}
  {{- if not $apiRefs }}{{ $empty = append $empty "gateway.api.parentRefs" }}{{ end -}}
  {{- if not $edgeRefs }}{{ $empty = append $empty "gateway.edge.parentRefs" }}{{ end -}}
  {{- if and $empty (ne .Values.service.type "LoadBalancer") -}}
    {{- $which := ternary "gateway.parentRefs is" (printf "%s (or gateway.parentRefs) is" (first $empty)) (eq (len $empty) 2) -}}
    {{- fail (printf "%s required: list the Gateway listeners that expose the controller, as {name, namespace, sectionName}. To expose the Service directly instead, set service.type=LoadBalancer; for an in-cluster-only install, set gateway.enabled=false." $which) -}}
  {{- end -}}
  {{- if and $gw.api.backendTLS.caCertificateRefs $gw.api.backendTLS.wellKnownCACertificates -}}
    {{- fail "gateway.api.backendTLS: set caCertificateRefs or wellKnownCACertificates, not both" -}}
  {{- end -}}
  {{- $edgeHosts := list .Values.edge.serverName -}}
  {{- with .Values.edge.allocationDomain }}{{ $edgeHosts = append $edgeHosts (printf "*.%s" .) }}{{ end -}}
  {{- $apiHosts := default (list .Values.edge.serverName) $gw.api.hostnames -}}
  {{- $caps := .Capabilities.APIVersions -}}
  {{- $missing := list -}}
  {{- $apiKinds := ternary (list "HTTPRoute" "BackendTLSPolicy") (list "TLSRoute") (eq $gw.api.mode "terminate") -}}
  {{- $apiOk := true -}}
  {{- if $apiRefs -}}
    {{- range $apiKinds -}}
      {{- if not ($caps.Has (printf "gateway.networking.k8s.io/v1/%s" .)) -}}
        {{- $apiOk = false -}}
        {{- if not (has . $missing) }}{{ $missing = append $missing . }}{{ end -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
  {{- $edgeOk := true -}}
  {{- if and $edgeRefs (not ($caps.Has "gateway.networking.k8s.io/v1/TLSRoute")) -}}
    {{- $edgeOk = false -}}
    {{- if not (has "TLSRoute" $missing) }}{{ $missing = append $missing "TLSRoute" }}{{ end -}}
  {{- end -}}
  {{- $_ := set $out "apiRefs" (ternary $apiRefs (list) $apiOk) -}}
  {{- $_ := set $out "edgeRefs" (ternary $edgeRefs (list) $edgeOk) -}}
  {{- $_ := set $out "apiHostnames" $apiHosts -}}
  {{- $_ := set $out "edgeHostnames" $edgeHosts -}}
  {{- $_ := set $out "missing" $missing -}}
  {{- $_ := set $out "envoyMissing" (and $gw.envoy.enabled (not ($caps.Has "gateway.envoyproxy.io/v1alpha1/BackendTrafficPolicy"))) -}}
{{- end -}}
{{- toJson $out -}}
{{- end -}}

{{/* Labels and annotations of every Gateway API object. */}}
{{- define "bkc.gateway.meta" -}}
labels:
  {{- include "bkc.labels" . | nindent 2 }}
  {{- with .Values.gateway.labels }}{{ toYaml . | nindent 2 }}{{ end }}
{{- with .Values.gateway.annotations }}
annotations: {{- toYaml . | nindent 2 }}
{{- end }}
{{- end -}}
