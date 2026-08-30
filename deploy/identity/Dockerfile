# Runtime image for the agentctl-identity service (RFC 0028): federated OIDC
# introspection, RFC 8628 device-flow login, per-(user,agent) A2A principal
# minting. Distroless nonroot; serves plain HTTP on :8087 (TLS terminates at
# the chart's mesh layer in P1).
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
RUN cargo zigbuild --release --target "$(cat /tmp/triple)" -p agentctl-identity --bin agentctl-identity \
 && cp "target/$(cat /tmp/triple)/release/agentctl-identity" /usr/local/bin/agentctl-identity

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /usr/local/bin/agentctl-identity /usr/local/bin/agentctl-identity
ENTRYPOINT ["/usr/local/bin/agentctl-identity"]
