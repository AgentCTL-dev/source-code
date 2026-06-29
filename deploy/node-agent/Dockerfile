# Runtime image for the node-agent (RFC 0008, Tier A). Host-side bridge: runs as
# root to read per-pod management sockets on the shared hostPath. The crate is
# `agentctl-node-agent` but its binary is named `node-agent`.
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
RUN cargo zigbuild --release --target "$(cat /tmp/triple)" -p agentctl-node-agent --bin node-agent \
 && cp "target/$(cat /tmp/triple)/release/node-agent" /usr/local/bin/node-agent

FROM gcr.io/distroless/cc-debian12
COPY --from=build /usr/local/bin/node-agent /usr/local/bin/node-agent
ENTRYPOINT ["/usr/local/bin/node-agent"]
