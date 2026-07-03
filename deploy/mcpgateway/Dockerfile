# Runtime image for the MCPGateway (RFC 0019 / 0021 §9). Distroless nonroot; serves
# the tool-plane broker (attest source-IP -> scope to MCPServerSet -> inject
# credential -> forward) on :8080 (health) + :8443 (mTLS).
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
RUN cargo zigbuild --release --target "$(cat /tmp/triple)" -p agentctl-mcpgateway --bin agentctl-mcpgateway \
 && cp "target/$(cat /tmp/triple)/release/agentctl-mcpgateway" /usr/local/bin/agentctl-mcpgateway

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /usr/local/bin/agentctl-mcpgateway /usr/local/bin/agentctl-mcpgateway
ENTRYPOINT ["/usr/local/bin/agentctl-mcpgateway"]
