#!/usr/bin/env bash
# Provision the validating webhook (RFC 0007): generate a CA + serving cert for
# the webhook Service, create the TLS Secret, deploy the webhook, and register
# the ValidatingWebhookConfiguration with the CA bundle. Run from the repo root.
set -euo pipefail

NS=agentctl-system
SVC=agentctl-admission
DIR="$(mktemp -d)"
trap 'rm -rf "$DIR"' EXIT

# Self-signed CA.
openssl genrsa -out "$DIR/ca.key" 2048
openssl req -x509 -new -nodes -key "$DIR/ca.key" -subj "/CN=agentctl-admission-ca" \
  -days 3650 -out "$DIR/ca.crt"

# Serving cert for the in-cluster Service DNS.
openssl genrsa -out "$DIR/tls.key" 2048
openssl req -new -key "$DIR/tls.key" -subj "/CN=$SVC.$NS.svc" -out "$DIR/tls.csr"
cat > "$DIR/ext.cnf" <<EOF
subjectAltName=DNS:$SVC.$NS.svc,DNS:$SVC.$NS.svc.cluster.local,DNS:$SVC.$NS
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -in "$DIR/tls.csr" -CA "$DIR/ca.crt" -CAkey "$DIR/ca.key" \
  -CAcreateserial -days 3650 -extfile "$DIR/ext.cnf" -out "$DIR/tls.crt"

# TLS Secret the webhook pod mounts.
kubectl -n "$NS" create secret tls agentctl-admission-tls \
  --cert="$DIR/tls.crt" --key="$DIR/tls.key" \
  --dry-run=client -o yaml | kubectl apply -f -

# RBAC + Deployment + Service.
kubectl apply -f deploy/admission/rbac.yaml
kubectl apply -f deploy/admission/deployment.yaml

# Register the webhook with the CA bundle that signed the serving cert.
CABUNDLE="$(base64 -w0 < "$DIR/ca.crt")"
sed "s|CA_BUNDLE_PLACEHOLDER|$CABUNDLE|" deploy/admission/webhook.yaml | kubectl apply -f -

echo "admission webhook installed; waiting for rollout…"
kubectl -n "$NS" rollout status deploy/agentctl-admission --timeout=90s
