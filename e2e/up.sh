#!/usr/bin/env bash
# Bring up the e2e cluster + its prerequisite add-ons, idempotently.
#
#   kind cluster (unless SKIP_BRINGUP) ── kind.yaml, or kind-calico.yaml when
#                                         E2E_CNI=calico (NetworkPolicy lane)
#   cert-manager                       ── required by the chart (waits webhook)
#   KEDA                               ── claim-fleet scale-from-zero (waits CRDs)
#   metrics-server                     ── `kubectl top` for the bench
#                                         (--kubelet-insecure-tls for kind)
#   kube-prometheus-stack              ── OPT-IN (E2E_PROMETHEUS=1): PromQL bench
#
# Re-runnable: every step is create-or-skip / install-or-upgrade. Honours
# KUBECONFIG and SKIP_BRINGUP=1 (then only the cluster create is skipped — the
# add-ons are still reconciled against the cluster KUBECONFIG points at; set
# E2E_SKIP_ADDONS=1 to skip those too on a cluster that already has them).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CLUSTER="${CLUSTER:-agentctl-e2e}"
E2E_CNI="${E2E_CNI:-kindnet}"             # kindnet | calico
CERT_MANAGER_VERSION="${CERT_MANAGER_VERSION:-v1.16.2}"
KEDA_VERSION="${KEDA_VERSION:-2.16.1}"    # helm chart appVersion
METRICS_SERVER_VERSION="${METRICS_SERVER_VERSION:-3.12.2}"
CALICO_VERSION="${CALICO_VERSION:-v3.28.2}"
KPS_VERSION="${KPS_VERSION:-65.5.0}"      # kube-prometheus-stack chart version

log() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }

wait_crd() { # wait_crd <crd-name>
  log "waiting for CRD $1"
  for _ in $(seq 1 60); do
    kubectl get crd "$1" >/dev/null 2>&1 && return 0
    sleep 2
  done
  echo "timed out waiting for CRD $1" >&2; return 1
}

# ---- 1. cluster ----------------------------------------------------------
if [ -n "${SKIP_BRINGUP:-}" ]; then
  log "SKIP_BRINGUP set — using the existing cluster (KUBECONFIG=${KUBECONFIG:-default})"
else
  if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    log "kind cluster '$CLUSTER' already exists — reusing"
  else
    if [ "$E2E_CNI" = "calico" ]; then
      log "creating kind cluster '$CLUSTER' (Calico lane, default CNI disabled)"
      kind create cluster --name "$CLUSTER" --config "$SCRIPT_DIR/kind-calico.yaml" --wait 120s || true
    else
      log "creating kind cluster '$CLUSTER'"
      kind create cluster --name "$CLUSTER" --config "$SCRIPT_DIR/kind.yaml" --wait 120s
    fi
  fi
fi

if [ -n "${E2E_SKIP_ADDONS:-}" ]; then
  log "E2E_SKIP_ADDONS set — skipping cert-manager/KEDA/metrics-server"
  exit 0
fi

# ---- 1b. Calico (NetworkPolicy lane only) --------------------------------
if [ "$E2E_CNI" = "calico" ] && [ -z "${SKIP_BRINGUP:-}" ]; then
  log "installing Calico $CALICO_VERSION (enforces NetworkPolicy)"
  kubectl apply --server-side -f "https://raw.githubusercontent.com/projectcalico/calico/${CALICO_VERSION}/manifests/calico.yaml"
  kubectl -n kube-system rollout status ds/calico-node --timeout=300s || true
fi

# ---- 2. cert-manager -----------------------------------------------------
log "installing cert-manager $CERT_MANAGER_VERSION"
kubectl apply -f "https://github.com/cert-manager/cert-manager/releases/download/${CERT_MANAGER_VERSION}/cert-manager.yaml"
log "waiting for cert-manager webhook to be Available"
kubectl -n cert-manager rollout status deploy/cert-manager-webhook --timeout=300s
kubectl -n cert-manager rollout status deploy/cert-manager --timeout=300s
kubectl -n cert-manager rollout status deploy/cert-manager-cainjector --timeout=300s

# ---- 3. KEDA -------------------------------------------------------------
log "installing KEDA (chart appVersion $KEDA_VERSION)"
helm repo add kedacore https://kedacore.github.io/charts >/dev/null 2>&1 || true
helm repo update kedacore >/dev/null
helm upgrade --install keda kedacore/keda \
  --namespace keda --create-namespace \
  --set "image.keda.tag=${KEDA_VERSION}" --wait --timeout 300s
wait_crd scaledobjects.keda.sh

# ---- 4. metrics-server ---------------------------------------------------
log "installing metrics-server $METRICS_SERVER_VERSION (--kubelet-insecure-tls for kind)"
helm repo add metrics-server https://kubernetes-sigs.github.io/metrics-server/ >/dev/null 2>&1 || true
helm repo update metrics-server >/dev/null
helm upgrade --install metrics-server metrics-server/metrics-server \
  --namespace kube-system \
  --version "$METRICS_SERVER_VERSION" \
  --set "args={--kubelet-insecure-tls,--kubelet-preferred-address-types=InternalIP}" \
  --wait --timeout 300s
kubectl -n kube-system rollout status deploy/metrics-server --timeout=300s

# ---- 5. kube-prometheus-stack (opt-in) -----------------------------------
if [ -n "${E2E_PROMETHEUS:-}" ]; then
  log "installing kube-prometheus-stack $KPS_VERSION (E2E_PROMETHEUS set)"
  helm repo add prometheus-community https://prometheus-community.github.io/helm-charts >/dev/null 2>&1 || true
  helm repo update prometheus-community >/dev/null
  helm upgrade --install prometheus prometheus-community/kube-prometheus-stack \
    --namespace monitoring --create-namespace \
    --version "$KPS_VERSION" \
    --set grafana.enabled=true \
    --wait --timeout 600s
  wait_crd servicemonitors.monitoring.coreos.com
else
  log "kube-prometheus-stack skipped (set E2E_PROMETHEUS=1 for the PromQL bench lane)"
fi

log "cluster is ready"
