#!/usr/bin/env bash
# Provision the mTLS material for the agent control surface (POST /mcp on :8443):
# one CA signs the serving cert and the control-plane client cert. The control
# plane presents the client cert to dial agent pods directly (the "Management"
# origin). Creates two Secrets in agentctl-system:
#   agentctl-agent-tls  {tls.crt, tls.key, ca.crt}  (server cert + client CA)
#   agentctl-client-tls      {tls.crt, tls.key, ca.crt}  (client cert + server CA)
# DEV certs — a real deployment uses cert-manager with rotation.
set -euo pipefail

NS=agentctl-system
DIR="$(mktemp -d)"
trap 'rm -rf "$DIR"' EXIT

# Self-signed CA.
openssl genrsa -out "$DIR/ca.key" 2048
openssl req -x509 -new -nodes -key "$DIR/ca.key" -subj "/CN=agentctl-mtls-ca" \
  -days 3650 -out "$DIR/ca.crt"

# Serving cert for the agent control surface (SAN is informational — clients verify the CA chain).
openssl genrsa -out "$DIR/server.key" 2048
openssl req -new -key "$DIR/server.key" -subj "/CN=agentctl-agent" -out "$DIR/server.csr"
printf 'subjectAltName=DNS:agentctl-agent\nextendedKeyUsage=serverAuth\n' > "$DIR/server.ext"
openssl x509 -req -in "$DIR/server.csr" -CA "$DIR/ca.crt" -CAkey "$DIR/ca.key" \
  -CAcreateserial -days 3650 -extfile "$DIR/server.ext" -out "$DIR/server.crt"

# control-plane client cert (apiserver + gateway present this).
openssl genrsa -out "$DIR/client.key" 2048
openssl req -new -key "$DIR/client.key" -subj "/CN=agentctl-control-plane" -out "$DIR/client.csr"
printf 'extendedKeyUsage=clientAuth\n' > "$DIR/client.ext"
openssl x509 -req -in "$DIR/client.csr" -CA "$DIR/ca.crt" -CAkey "$DIR/ca.key" \
  -CAcreateserial -days 3650 -extfile "$DIR/client.ext" -out "$DIR/client.crt"

kubectl -n "$NS" create secret generic agentctl-agent-tls \
  --from-file=tls.crt="$DIR/server.crt" --from-file=tls.key="$DIR/server.key" \
  --from-file=ca.crt="$DIR/ca.crt" --dry-run=client -o yaml | kubectl apply -f -
kubectl -n "$NS" create secret generic agentctl-client-tls \
  --from-file=tls.crt="$DIR/client.crt" --from-file=tls.key="$DIR/client.key" \
  --from-file=ca.crt="$DIR/ca.crt" --dry-run=client -o yaml | kubectl apply -f -

echo "mTLS secrets agentctl-agent-tls + agentctl-client-tls created in $NS"
