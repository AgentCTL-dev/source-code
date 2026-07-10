#!/usr/bin/env bash
# Build (or pull) every image the e2e suite needs and make it available to the
# cluster. Two delivery modes:
#
#   default            `kind load docker-image` into the kind cluster (CLUSTER).
#   REGISTRY=<host/ns> docker push <REGISTRY>/<name> (cluster-portable — use this
#                      with SKIP_BRINGUP=1 against a real cluster; install.sh /
#                      the chart's image.registry must then point at REGISTRY).
#
# Images produced (contract 1.0):
#   agentd:1.0.0                 the real reference agent (built from
#                                $AGENTD_SRC/Dockerfile — serves mTLS HTTPS /mcp,
#                                dials providers/MCP servers directly). Built from
#                                source to match the exact contract-1.0 build under
#                                test; the published ghcr.io/agentd-dev/agentd:1.0.0
#                                is used when AGENTD_GHCR is set.
#   mock-agent:dev               conformant-agent stand-in (mTLS HTTPS self-MCP).
#   agentctl/<comp>:dev          the 6 control-plane components, each from
#                                deploy/<comp>/Dockerfile (a control-plane component
#                                dir, not the reference agent).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CLUSTER="${CLUSTER:-agentctl-e2e}"
TAG="${TAG:-dev}"
REGISTRY="${REGISTRY:-}"                       # empty = kind load; set = push
AGENTD_SRC="${AGENTD_SRC:-/root/agentd-dev/source-code}"
AGENTD_IMAGE="${AGENTD_IMAGE:-agentd:1.0.0}"
AGENTD_GHCR="${AGENTD_GHCR:-ghcr.io/agentd-dev/agentd:1.0.0}"

# The 6 control-plane components (component == deploy/<comp>/Dockerfile dir),
# matching the release.yml build matrix. Contract 1.0: agents serve HTTPS MCP
# natively and dial their model provider + MCP servers directly (no broker).
COMPONENTS=(operator apiserver gateway admission coordination scaler)

log() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }

# build <image> <dockerfile> <context> [extra docker build args...]
build() {
  local image="$1" dockerfile="$2" context="$3"; shift 3
  log "building $image"
  docker buildx build --load -t "$image" -f "$dockerfile" "$@" "$context"
}

# publish <image> — kind load by default, or tag+push when REGISTRY is set.
publish() {
  local image="$1"
  if [ -n "$REGISTRY" ]; then
    local remote="$REGISTRY/$image"
    log "pushing $remote"
    docker tag "$image" "$remote"
    docker push "$remote"
  else
    log "kind load $image -> $CLUSTER"
    kind load docker-image "$image" --name "$CLUSTER"
  fi
}

# ---- agentd (contract 1.0) -----------------------------------------------
if [ -n "${AGENTD_PULL:-}" ] || [ ! -f "$AGENTD_SRC/Dockerfile" ]; then
  log "pulling $AGENTD_GHCR -> $AGENTD_IMAGE"
  docker pull "$AGENTD_GHCR"
  docker tag "$AGENTD_GHCR" "$AGENTD_IMAGE"
else
  build "$AGENTD_IMAGE" "$AGENTD_SRC/Dockerfile" "$AGENTD_SRC"
fi
publish "$AGENTD_IMAGE"

# ---- mock-agent:dev ------------------------------------------------------
# The mock-agent Dockerfile COPYs target/release/mock-agent, so build the host
# binary first (release profile, matching deploy/examples/mock-agent.Dockerfile).
log "cargo build --release -p mock-agent"
( cd "$REPO_ROOT" && cargo build --release -p mock-agent )
build "mock-agent:$TAG" "$REPO_ROOT/deploy/examples/mock-agent.Dockerfile" "$REPO_ROOT"
publish "mock-agent:$TAG"

# ---- mock-aauth-mcp:dev --------------------------------------------------
# The AAuth-verifying remote-MCP fixture (RFC 0024 phase 0): verifies RFC 9421
# signatures + agent tokens against the e2e Agent Provider (apd). Same crate,
# second binary (already built by the -p mock-agent build above).
build "mock-aauth-mcp:$TAG" "$REPO_ROOT/deploy/examples/mock-aauth-mcp.Dockerfile" "$REPO_ROOT"
publish "mock-aauth-mcp:$TAG"

# ---- apd:e2e (the Agent Provider — "the house", RFC 0023) ------------------
# Built from the sibling agentprovider checkout when present (hermetic, like
# agentd); skipped-with-warning otherwise — the aauth e2e scenario then SKIPs.
APD_SRC="${APD_SRC:-/root/agentprovider/source-code}"
APD_IMAGE="${APD_IMAGE:-apd:e2e}"
if [ -f "$APD_SRC/Dockerfile" ]; then
  build "$APD_IMAGE" "$APD_SRC/Dockerfile" "$APD_SRC"
  publish "$APD_IMAGE"
else
  log "WARN: $APD_SRC not found — skipping $APD_IMAGE (aauth scenario will SKIP)"
fi

# ---- the 6 control-plane images -----------------------------------------
for comp in "${COMPONENTS[@]}"; do
  build "agentctl/$comp:$TAG" "$REPO_ROOT/deploy/$comp/Dockerfile" "$REPO_ROOT"
  publish "agentctl/$comp:$TAG"
done

# The v1 stdio->HTTP work.* bridge is RETIRED in contract 1.0 — agentd speaks
# HTTPS MCP natively, so claim-mode agents dial the coordination
# server's /mcp directly.

log "images ready"
