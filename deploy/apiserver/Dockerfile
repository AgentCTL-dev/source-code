# Runtime image for the agentctl aggregated apiserver (RFC 0009). Distroless
# nonroot; serves HTTPS on 6443 with the mounted serving cert.
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
RUN cargo zigbuild --release --target "$(cat /tmp/triple)" -p agentctl-apiserver --bin agentctl-apiserver \
 && cp "target/$(cat /tmp/triple)/release/agentctl-apiserver" /usr/local/bin/agentctl-apiserver

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /usr/local/bin/agentctl-apiserver /usr/local/bin/agentctl-apiserver
ENTRYPOINT ["/usr/local/bin/agentctl-apiserver"]
