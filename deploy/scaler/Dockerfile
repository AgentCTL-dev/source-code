# Runtime image for the KEDA external scaler. Distroless nonroot;
# serves the KEDA externalscaler gRPC API on :9100 and a small HTTP surface
# (/healthz, /readyz, /metrics) on :8080.
#
# Multi-arch via cross-compilation (cargo-zigbuild on $BUILDPLATFORM -> $TARGETPLATFORM).
# See deploy/operator/Dockerfile for the annotated copy.
#
# NOTE: the scaler's tonic-build step needs `protoc` (the protobuf compiler) at
# build time. We install protobuf-compiler in the builder stage so the image
# builds regardless of whether the crate vendors protoc itself.

FROM --platform=$BUILDPLATFORM messense/cargo-zigbuild:0.23.0 AS build
WORKDIR /build
# protoc for tonic-build (gRPC codegen) — apt is available in the zigbuild base.
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") echo x86_64-unknown-linux-gnu ;; \
      "linux/arm64") echo aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETPLATFORM=$TARGETPLATFORM" >&2; exit 1 ;; \
    esac > /tmp/triple && rustup target add "$(cat /tmp/triple)"
COPY . .
RUN cargo zigbuild --release --target "$(cat /tmp/triple)" -p agentctl-scaler --bin agentctl-scaler \
 && cp "target/$(cat /tmp/triple)/release/agentctl-scaler" /usr/local/bin/agentctl-scaler

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /usr/local/bin/agentctl-scaler /usr/local/bin/agentctl-scaler
ENTRYPOINT ["/usr/local/bin/agentctl-scaler"]
