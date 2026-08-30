#!/usr/bin/env bash
# images-quick.sh — the fast, host-build image path for local e2e iteration.
#
# The canonical images.sh builds everything hermetically inside docker
# (cargo-zigbuild, multi-arch) — right for CI, slow for a laptop loop: a cold
# in-container build of agentd alone is tens of minutes. This variant reuses
# the HOST toolchain's incremental target/ and assembles runtime images by
# COPYing host binaries onto the same runtime bases the canonical images use
# (distroless/cc glibc ⟷ host x86_64-gnu binaries). Behavior-identical for the
# e2e suite; NOT the artifact we'd publish.
#
#   AGENTD_BIN   prebuilt agentd binary   (default /root/agentd-dev/agentd-1.3.1.bin)
#   AGENTD_TAG   agentd image tag         (default 1.3.1)
#   TAG          control-plane image tag  (default dev)
#   CLUSTER      kind cluster name        (default agentctl-e2e)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLUSTER="${CLUSTER:-agentctl-e2e}"
TAG="${TAG:-dev}"
AGENTD_BIN="${AGENTD_BIN:-/root/agentd-dev/agentd-1.3.1.bin}"
AGENTD_TAG="${AGENTD_TAG:-1.3.1}"
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGE_DIR"' EXIT

log() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }

build_load() { # <image> <dockerfile-dir>
  docker buildx build --load -t "$1" "$2" >/dev/null
  kind load docker-image "$1" --name "$CLUSTER"
}

[ -f "$AGENTD_BIN" ] || { echo "AGENTD_BIN not found: $AGENTD_BIN" >&2; exit 1; }

# ---- agentd (prebuilt host binary on the glibc runtime base) --------------
log "agentd:$AGENTD_TAG from $AGENTD_BIN"
mkdir -p "$STAGE_DIR/agentd"
cp "$AGENTD_BIN" "$STAGE_DIR/agentd/agentd"
cat > "$STAGE_DIR/agentd/Dockerfile" <<'EOF'
# Quick-path agentd: host gnu binary on distroless/cc (glibc + CA roots),
# nonroot — the same runtime posture as the published scratch/musl image for
# everything the e2e suite exercises.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY agentd /agentd
ENTRYPOINT ["/agentd"]
EOF
build_load "agentd:$AGENTD_TAG" "$STAGE_DIR/agentd"

# ---- agentd-staging (binary + shell, for admission rung 4) ----------------
log "agentd-staging:$AGENTD_TAG"
mkdir -p "$STAGE_DIR/staging"
cp "$AGENTD_BIN" "$STAGE_DIR/staging/agentd"
cat > "$STAGE_DIR/staging/Dockerfile" <<'EOF'
FROM debian:stable-slim
COPY agentd /agentd
USER 65532:65532
ENTRYPOINT ["/agentd"]
EOF
build_load "agentd-staging:$AGENTD_TAG" "$STAGE_DIR/staging"

# ---- control-plane components + mocks (host release build) ----------------
log "cargo build --release (control plane + mocks)"
( cd "$REPO_ROOT" && cargo build --release \
    -p agentctl-operator -p agentctl-apiserver -p agentctl-gateway \
    -p agentctl-admission -p agentctl-coordination -p agentctl-scaler \
    -p agentctl-identity -p mock-agent )

comp_image() { # <component> <binary-name>
  local comp="$1" bin="$2"
  log "agentctl/$comp:$TAG"
  mkdir -p "$STAGE_DIR/$comp"
  cp "$REPO_ROOT/target/release/$bin" "$STAGE_DIR/$comp/$bin"
  cat > "$STAGE_DIR/$comp/Dockerfile" <<EOF
FROM gcr.io/distroless/cc-debian12:nonroot
COPY $bin /usr/local/bin/$bin
ENTRYPOINT ["/usr/local/bin/$bin"]
EOF
  build_load "agentctl/$comp:$TAG" "$STAGE_DIR/$comp"
}

comp_image operator agentctl-operator
comp_image apiserver agentctl-apiserver
comp_image gateway agentctl-gateway
comp_image admission agentctl-admission
comp_image coordination agentctl-coordination
comp_image scaler agentctl-scaler
comp_image identity agentctl-identity

# mock-agent + mock-aauth-mcp: staged like the components (the repo-root
# .dockerignore excludes target/, so the canonical Dockerfiles cannot see the
# host binaries from a repo-root context).
mock_image() { # <image-name> <binary>
  local img="$1" bin="$2"
  log "$img:$TAG"
  mkdir -p "$STAGE_DIR/$bin"
  cp "$REPO_ROOT/target/release/$bin" "$STAGE_DIR/$bin/$bin"
  cat > "$STAGE_DIR/$bin/Dockerfile" <<EOF
FROM gcr.io/distroless/cc-debian12
COPY $bin /usr/local/bin/$bin
ENTRYPOINT ["/usr/local/bin/$bin"]
EOF
  build_load "$img:$TAG" "$STAGE_DIR/$bin"
}
mock_image mock-agent mock-agent
mock_image mock-aauth-mcp mock-aauth-mcp

log "done — quick images loaded into kind/$CLUSTER"
