{{/*
What the chart renders for Gateway API exposure, as JSON:
  apiRefs, edgeRefs   parentRefs per route; [] = that route is not rendered
  apiHostnames        HTTPRoute or TLSRoute hostnames of the API
  edgeHostnames       TLSRoute hostnames (SNI) of the edge
  missing             kinds the routes need that the cluster does not serve
  envoyMissing        gateway.envoy.enabled but no BackendTrafficPolicy CRD
  apiUnpinned         an HTTPRoute parentRef names no sectionName or port
Fails when a route has no parentRefs and service.type is not LoadBalancer, and in
passthrough mode when both TLSRoutes could match one SNI on one listener.
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
  {{- /* Passthrough: two TLSRoutes on one listener with overlapping SNI would let the
         gateway send edge connections to the API port. */ -}}
  {{- if eq $gw.api.mode "passthrough" -}}
    {{- range $a := $apiRefs -}}
      {{- range $e := $edgeRefs -}}
        {{- $same := and (eq $a.name $e.name) (eq (default "" $a.namespace) (default "" $e.namespace)) (eq (default "Gateway" $a.kind) (default "Gateway" $e.kind)) -}}
        {{- $sameSection := or (not $a.sectionName) (not $e.sectionName) (eq $a.sectionName $e.sectionName) -}}
        {{- $samePort := or (not $a.port) (not $e.port) (eq (int $a.port) (int $e.port)) -}}
        {{- if and $same $sameSection $samePort -}}
          {{- range $ha := $apiHosts -}}
            {{- range $he := $edgeHosts -}}
              {{- $da := trimPrefix "*." $ha -}}
              {{- $de := trimPrefix "*." $he -}}
              {{- if or (eq $ha $he) (and (hasPrefix "*." $ha) (hasSuffix (printf ".%s" $da) $he)) (and (hasPrefix "*." $he) (hasSuffix (printf ".%s" $de) $ha)) -}}
                {{- fail (printf "gateway.api.mode=passthrough: the API and edge TLSRoutes would both match SNI %s on Gateway %s listener %s, so edge connections could reach the API. Attach them to different listeners (gateway.api.parentRefs and gateway.edge.parentRefs with sectionName) or give the API other gateway.api.hostnames." $ha $a.name (default "(any)" (default $e.sectionName $a.sectionName))) -}}
              {{- end -}}
            {{- end -}}
          {{- end -}}
        {{- end -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
  {{- /* Terminate: an HTTPRoute ref with no sectionName or port attaches to every HTTP
         and HTTPS listener that admits it, plain HTTP included. */ -}}
  {{- $unpinned := false -}}
  {{- if eq $gw.api.mode "terminate" -}}
    {{- range $apiRefs -}}
      {{- if not (or .sectionName .port) }}{{ $unpinned = true }}{{ end -}}
    {{- end -}}
  {{- end -}}
  {{- $_ := set $out "apiUnpinned" $unpinned -}}
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
