#!/usr/bin/env bash
# Build (or pull) every image the e2e suite needs and make it available to the
# cluster. Two delivery modes:
#
#   default            `kind load docker-image` into the kind cluster (CLUSTER).
#   REGISTRY=<host/ns> docker push <REGISTRY>/<name> (cluster-portable — use this
#                      with SKIP_BRINGUP=1 against a real cluster; install.sh /
#                      the chart's image.registry must then point at REGISTRY).
#
# Images produced (contract 2.0):
#   agentd:2.x                   the real reference agent (built from
#                                $AGENTD_SRC/Dockerfile — serves mTLS HTTPS /mcp,
#                                dials gateways keyless). No 2.x tag is published on
#                                GHCR yet (RFC 0021 §14), so this builds from source.
#   mock-agent:dev               conformant-agent stand-in (mTLS HTTPS self-MCP).
#   agentctl/<comp>:dev          the 8 control-plane components, each from
#                                deploy/<comp>/Dockerfile (v2: mcpgateway, not the
#                                retired node-agent).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CLUSTER="${CLUSTER:-agentctl-e2e}"
TAG="${TAG:-dev}"
REGISTRY="${REGISTRY:-}"                       # empty = kind load; set = push
AGENTD_SRC="${AGENTD_SRC:-/root/agentd-dev/source-code}"
AGENTD_IMAGE="${AGENTD_IMAGE:-agentd:2.x}"
AGENTD_GHCR="${AGENTD_GHCR:-ghcr.io/agentd-dev/agentd:2.x}"

# The 8 control-plane components (component == deploy/<comp>/Dockerfile dir),
# matching the release.yml build matrix. Contract 2.0: the node-agent is retired;
# the mcpgateway is the tool-plane broker.
COMPONENTS=(operator mcpgateway apiserver gateway modelgateway admission coordination scaler)

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

# ---- agentd (contract 2.0) -----------------------------------------------
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

# ---- the 8 control-plane images -----------------------------------------
for comp in "${COMPONENTS[@]}"; do
  build "agentctl/$comp:$TAG" "$REPO_ROOT/deploy/$comp/Dockerfile" "$REPO_ROOT"
  publish "agentctl/$comp:$TAG"
done

# The v1 stdio->HTTP work.* bridge is RETIRED in contract 2.0 — agentd speaks
# HTTPS MCP natively (RFC 0021 §9), so claim-mode agents dial the coordination
# server's /mcp directly (no bridge sidecar, no hostPath socket).

log "images ready"
