#!/usr/bin/env bash
# helm install/upgrade the agentctl chart for e2e, with the base values plus any
# overlays passed as arguments. Each arg is an overlay path: an absolute/relative
# file, or a bare name resolved against e2e/values/ (with or without .yaml). E.g.
#   bash install.sh sec-netpol                 # values/sec-netpol.yaml
#   bash install.sh values/store-postgres.yaml # explicit path
#   bash install.sh sec-coord-mtls sec-coord-attest
#
# image.registry defaults to "" so the chart renders the LOCAL (kind-loaded)
# image names (agentctl/<comp>:dev). Set REGISTRY to the same value images.sh
# pushed to for the cluster-portable (real-cluster) path.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

RELEASE="${RELEASE:-agentctl}"
NAMESPACE="${NAMESPACE:-agentctl-system}"
CHART="${CHART:-$REPO_ROOT/charts/agentctl}"
TAG="${TAG:-dev}"
REGISTRY="${REGISTRY:-}"            # "" = local kind-loaded names

log() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }

# Resolve each overlay arg into a -f flag.
VALUE_FLAGS=(-f "$SCRIPT_DIR/values/e2e-base.yaml")
for ov in "$@"; do
  if [ -f "$ov" ]; then
    f="$ov"
  elif [ -f "$SCRIPT_DIR/values/$ov" ]; then
    f="$SCRIPT_DIR/values/$ov"
  elif [ -f "$SCRIPT_DIR/values/$ov.yaml" ]; then
    f="$SCRIPT_DIR/values/$ov.yaml"
  else
    echo "overlay not found: $ov" >&2; exit 1
  fi
  VALUE_FLAGS+=(-f "$f")
done

# ---- namespace (baseline PSA — no control-plane component needs privileged,
# so no control-plane component needs hostPath/hostPID/privileged) ----------
log "ensuring namespace $NAMESPACE (baseline PodSecurity)"
kubectl get ns "$NAMESPACE" >/dev/null 2>&1 || kubectl create ns "$NAMESPACE"
kubectl label ns "$NAMESPACE" pod-security.kubernetes.io/enforce=baseline --overwrite

# ---- helm upgrade --install ----------------------------------------------
log "helm upgrade --install $RELEASE -> $NAMESPACE"
helm upgrade --install "$RELEASE" "$CHART" \
  --namespace "$NAMESPACE" \
  --set "image.registry=$REGISTRY" \
  --set "image.tag=$TAG" \
  "${VALUE_FLAGS[@]}" \
  --wait --timeout 300s

log "installed; control-plane deployments:"
kubectl -n "$NAMESPACE" get deploy,ds,sts 2>/dev/null || true
