#!/usr/bin/env bash
# Install the agentctl aggregated apiserver: generate a serving CA + cert (with
# the Service DNS SANs), create the TLS Secret, apply the Deployment/Service, and
# register the APIService with the CA in its caBundle.
#
# Prereqs: the namespace (deploy/operator) exists; the image agentctl/apiserver:dev
# is loaded (kind load). Run from the repo root.
set -euo pipefail

NS=agentctl-system
SVC=agentctl-apiserver
DIR="$(mktemp -d)"
trap 'rm -rf "$DIR"' EXIT

echo "==> generating serving CA + cert (SANs for ${SVC}.${NS}.svc[.cluster.local])"
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$DIR/ca.key" -out "$DIR/ca.crt" -subj "/CN=agentctl-apiserver-ca" >/dev/null 2>&1

cat > "$DIR/san.cnf" <<EOF
[req]
req_extensions = v3_req
distinguished_name = dn
[dn]
[v3_req]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = @alt
[alt]
DNS.1 = ${SVC}.${NS}.svc
DNS.2 = ${SVC}.${NS}.svc.cluster.local
EOF

openssl req -newkey rsa:2048 -nodes -keyout "$DIR/tls.key" -out "$DIR/tls.csr" \
  -subj "/CN=${SVC}.${NS}.svc" -config "$DIR/san.cnf" >/dev/null 2>&1
openssl x509 -req -in "$DIR/tls.csr" -CA "$DIR/ca.crt" -CAkey "$DIR/ca.key" -CAcreateserial \
  -days 3650 -out "$DIR/tls.crt" -extensions v3_req -extfile "$DIR/san.cnf" >/dev/null 2>&1

echo "==> creating TLS secret"
kubectl -n "$NS" create secret tls "${SVC}-tls" \
  --cert="$DIR/tls.crt" --key="$DIR/tls.key" \
  --dry-run=client -o yaml | kubectl apply -f -

echo "==> applying RBAC (ServiceAccount + auth-delegator + authreader)"
kubectl apply -f deploy/apiserver/rbac.yaml

echo "==> applying Deployment + Service"
kubectl apply -f deploy/apiserver/deployment.yaml

echo "==> registering APIService (caBundle = serving CA)"
CABUNDLE="$(base64 -w0 < "$DIR/ca.crt")"
sed "s|CABUNDLE_PLACEHOLDER|${CABUNDLE}|" deploy/apiserver/apiservice.yaml | kubectl apply -f -

echo "==> done. check: kubectl get apiservices v1alpha1.management.agents.x-k8s.io"
