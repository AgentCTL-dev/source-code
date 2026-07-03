# Operator Lifecycle Manager (OLM) bundle image for agentctl (alpha/preview).
#
# A bundle image is a scratch image that carries only the operator metadata
# (manifests + metadata) — it is never run; OLM reads its labels + layers.
# The LABELs below MUST stay in sync with bundle/metadata/annotations.yaml.
#
# Build (context = this bundle/ directory):
#   docker build -f bundle/bundle.Dockerfile -t ghcr.io/agentctl-dev/agentctl-bundle:1.0.0 bundle
# Validate:
#   operator-sdk bundle validate ./bundle
FROM scratch

# Core OLM bundle labels (must match metadata/annotations.yaml).
LABEL operators.operatorframework.io.bundle.mediatype.v1=registry+v1
LABEL operators.operatorframework.io.bundle.manifests.v1=manifests/
LABEL operators.operatorframework.io.bundle.metadata.v1=metadata/
LABEL operators.operatorframework.io.bundle.package.v1=agentctl
LABEL operators.operatorframework.io.bundle.channels.v1=alpha
LABEL operators.operatorframework.io.bundle.channel.default.v1=alpha
LABEL operators.operatorframework.io.metrics.builder=operator-sdk-manual
LABEL operators.operatorframework.io.metrics.mediatype.v1=metrics+v1

# Copy the bundle content into the conventional locations.
COPY manifests /manifests/
COPY metadata /metadata/
