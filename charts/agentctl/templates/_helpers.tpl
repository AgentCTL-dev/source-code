{{/* Namespace the control plane runs in. */}}
{{- define "agentctl.namespace" -}}{{ .Values.namespace.name }}{{- end -}}

{{/* Common labels for every object. */}}
{{- define "agentctl.labels" -}}
app.kubernetes.io/part-of: agentctl
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end -}}

{{/*
Resolve a component image. Usage:
  {{ include "agentctl.image" (dict "root" $ "component" "operator") }}
digest pinned -> "<registry>/<component>@<digest>" (image.digests[component] wins);
registry set  -> "<registry>/<component>:<tag>";
registry empty -> the local dev name "agentctl/<component>:<tag>" (kind-loaded).
*/}}
{{- define "agentctl.image" -}}
{{- $reg := .root.Values.image.registry -}}
{{- $tag := .root.Values.image.tag -}}
{{- $digest := "" -}}
{{- with .root.Values.image.digests -}}{{- $digest = index . $.component | default "" -}}{{- end -}}
{{- if and $reg $digest -}}{{ $reg }}/{{ .component }}@{{ $digest }}{{- else if $reg -}}{{ $reg }}/{{ .component }}:{{ $tag }}{{- else -}}agentctl/{{ .component }}:{{ $tag }}{{- end -}}
{{- end -}}

{{/* imagePullSecrets block (if any). Usage: {{ include "agentctl.pullSecrets" $ | nindent 6 }} */}}
{{- define "agentctl.pullSecrets" -}}
{{- with .Values.image.pullSecrets }}
imagePullSecrets:
{{- range . }}
  - name: {{ . }}
{{- end }}
{{- end }}
{{- end -}}

{{/*
The DATABASE_URL env entry for the durable store (gateway + modelgateway).
bundled -> the chart's agentctl-postgres secret; external -> the user's DSN secret.
Usage: {{ include "agentctl.databaseUrlEnv" $ | nindent 12 }}
*/}}
{{- define "agentctl.databaseUrlEnv" -}}
- name: DATABASE_URL
  valueFrom:
    secretKeyRef:
{{- if eq .Values.postgres.mode "bundled" }}
      name: agentctl-postgres
      key: DATABASE_URL
{{- else }}
      name: {{ required "postgres.external.dsnSecretName is required when postgres.mode=external" .Values.postgres.external.dsnSecretName }}
      key: {{ .Values.postgres.external.dsnSecretKey }}
{{- end }}
{{- end -}}

{{/* The CA issuer the leaf Certificates reference (the chart's CA, or a user issuer). */}}
{{- define "agentctl.caIssuer" -}}
{{- if .Values.certManager.caIssuerRef -}}
name: {{ .Values.certManager.caIssuerRef }}
kind: ClusterIssuer
{{- else -}}
name: agentctl-ca
kind: Issuer
{{- end -}}
{{- end -}}

{{/*
Pod-spec-level scheduling block for a component. Usage (place at pod-spec level):
  {{- with (include "agentctl.scheduling" (dict "root" $ "comp" "operator")) }}
  {{- . | nindent 6 }}
  {{- end }}
Renders only the knobs the component sets (nodeSelector, affinity, tolerations,
topologySpreadConstraints, priorityClassName); empty -> "" so callers can guard
with `with` and emit nothing for the default install.
*/}}
{{- define "agentctl.scheduling" -}}
{{- $c := index .root.Values .comp | default dict -}}
{{- with $c.nodeSelector }}
nodeSelector:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $c.affinity }}
affinity:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $c.tolerations }}
tolerations:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $c.topologySpreadConstraints }}
topologySpreadConstraints:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $c.priorityClassName }}
priorityClassName: {{ . }}
{{- end }}
{{- end -}}

{{/*
Container env entries for a component: RUST_LOG (from <comp>.logLevel, default
info) followed by any <comp>.extraEnv. envFrom is rendered separately by the
template via {{- with $c.envFrom }}. Usage (at container env: level):
  env:
    {{- include "agentctl.podEnv" (dict "root" $ "comp" "operator") | nindent 12 }}
*/}}
{{- define "agentctl.podEnv" -}}
{{- $c := index .root.Values .comp | default dict -}}
- name: RUST_LOG
  value: {{ $c.logLevel | default "info" | quote }}
{{- with $c.extraEnv }}
{{ toYaml . | trimSuffix "\n" }}
{{- end -}}
{{- end -}}

{{/*
Global commonLabels as YAML, for callers to merge alongside agentctl.labels on
object metadata. Usage (guard so the default empty map emits nothing):
  {{- with (include "agentctl.commonLabels" $) }}
  {{- . | nindent 4 }}
  {{- end }}
*/}}
{{- define "agentctl.commonLabels" -}}
{{- with .Values.commonLabels }}{{ toYaml . | trimSuffix "\n" }}{{- end }}
{{- end -}}
