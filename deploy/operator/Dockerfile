# Runtime image for the agentctl operator.
#
# Multi-arch via CROSS-COMPILATION (no QEMU emulation of the compiler): the build
# stage always runs on the native build arch ($BUILDPLATFORM) and cross-compiles
# to $TARGETPLATFORM with cargo-zigbuild. Runtime is distroless/cc (glibc +
# ca-certificates, nonroot uid 65532); the binary uses rustls (no OpenSSL), so cc
# is sufficient. CI caches BuildKit layers across runs (--cache-to/from gha).
# Build from the repo root:
#
#   docker buildx build --platform linux/amd64,linux/arm64 \
#     -f deploy/operator/Dockerfile -t agentctl/operator:dev .

FROM --platform=$BUILDPLATFORM messense/cargo-zigbuild:0.23.0 AS build
WORKDIR /build
ARG TARGETPLATFORM
# Map the OCI platform to a Rust gnu target triple (matches distroless/cc glibc).
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") echo x86_64-unknown-linux-gnu ;; \
      "linux/arm64") echo aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETPLATFORM=$TARGETPLATFORM" >&2; exit 1 ;; \
    esac > /tmp/triple && rustup target add "$(cat /tmp/triple)"
COPY . .
RUN cargo zigbuild --release --target "$(cat /tmp/triple)" -p agentctl-operator --bin agentctl-operator \
 && cp "target/$(cat /tmp/triple)/release/agentctl-operator" /usr/local/bin/agentctl-operator

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /usr/local/bin/agentctl-operator /usr/local/bin/agentctl-operator
ENTRYPOINT ["/usr/local/bin/agentctl-operator"]
