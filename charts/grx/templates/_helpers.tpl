{{/*
Common helpers (fullname, labels, selectorLabels, serviceAccountName).
Pattern matches the helm-create scaffold conventions so operators
familiar with bitnami / nginx-ingress charts see no surprises.
*/}}

{{- define "grx.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "grx.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "grx.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels — Kubernetes-recommended labels (app.kubernetes.io/*).
*/}}
{{- define "grx.labels" -}}
helm.sh/chart: {{ include "grx.chart" . }}
{{ include "grx.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: graphreact
{{- end -}}

{{- define "grx.selectorLabels" -}}
app.kubernetes.io/name: {{ include "grx.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
ServiceAccount name: either `serviceAccount.name` or a fullname-derived
default.
*/}}
{{- define "grx.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "grx.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Image reference. Prefers `.Values.image.tag`, falls back to
`.Chart.AppVersion`.
*/}}
{{- define "grx.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}
