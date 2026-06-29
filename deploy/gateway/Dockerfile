# Runtime image for the A2A gateway (RFC 0013). Distroless nonroot; serves the
# A2A HTTP surface on :8080.
#
# Multi-arch via cross-compilation (cargo-zigbuild on $BUILDPLATFORM -> $TARGETPLATFORM).
# See deploy/operator/Dockerfile for the annotated copy.

FROM --platform=$BUILDPLATFORM messense/cargo-zigbuild:0.23.0 AS build
WORKDIR /build
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") echo x86_64-unknown-linux-gnu ;; \
      "linux/arm64") echo aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETPLATFORM=$TARGETPLATFORM" >&2; exit 1 ;; \
    esac > /tmp/triple && rustup target add "$(cat /tmp/triple)"
COPY . .
RUN cargo zigbuild --release --target "$(cat /tmp/triple)" -p agentctl-gateway --bin agentctl-gateway \
 && cp "target/$(cat /tmp/triple)/release/agentctl-gateway" /usr/local/bin/agentctl-gateway

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /usr/local/bin/agentctl-gateway /usr/local/bin/agentctl-gateway
ENTRYPOINT ["/usr/local/bin/agentctl-gateway"]
