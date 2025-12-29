{{/*
Expand the name of the chart.
*/}}
{{- define "buildkit-controller.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "buildkit-controller.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "buildkit-controller.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "buildkit-controller.labels" -}}
helm.sh/chart: {{ include "buildkit-controller.chart" . }}
{{ include "buildkit-controller.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- range $key, $value := .Values.labels }}
{{ $key }}: {{ $value | quote }}
{{- end }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "buildkit-controller.selectorLabels" -}}
app.kubernetes.io/name: {{ include "buildkit-controller.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "buildkit-controller.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "buildkit-controller.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Namespace
*/}}
{{- define "buildkit-controller.namespace" -}}
{{- .Values.namespace.name | default "buildkit-system" }}
{{- end }}

{{/*
Construct full controller image path from registry, name, and tag
*/}}
{{- define "buildkit-controller.image" -}}
{{- if .Values.image.registry }}
{{- printf "%s/%s:%s" .Values.image.registry .Values.image.name .Values.image.tag }}
{{- else }}
{{- printf "%s:%s" .Values.image.name .Values.image.tag }}
{{- end }}
{{- end }}

{{/*
Gateway image name.
*/}}
{{- define "buildkit-controller.gatewayImage" -}}
{{- if .Values.gateway.image.registry }}
{{- printf "%s/%s:%s" .Values.gateway.image.registry .Values.gateway.image.name .Values.gateway.image.tag }}
{{- else }}
{{- printf "%s:%s" .Values.gateway.image.name .Values.gateway.image.tag }}
{{- end }}
{{- end }}