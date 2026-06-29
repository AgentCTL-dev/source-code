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
registry empty -> the local dev name "agentctl/<component>:<tag>" (kind-loaded);
registry set   -> "<registry>/<component>:<tag>".
*/}}
{{- define "agentctl.image" -}}
{{- $reg := .root.Values.image.registry -}}
{{- $tag := .root.Values.image.tag -}}
{{- if $reg -}}{{ $reg }}/{{ .component }}:{{ $tag }}{{- else -}}agentctl/{{ .component }}:{{ $tag }}{{- end -}}
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
