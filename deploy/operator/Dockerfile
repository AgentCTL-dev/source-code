# Runtime image for the agentctl operator.
#
# Self-contained multi-stage build: the release binary is compiled inside the
# image (no host-built binary required, so CI needs no prior `cargo build`).
# The runtime is distroless/cc (glibc + ca-certificates, runs as nonroot uid
# 65532). The binary uses rustls (no OpenSSL), so cc is sufficient. Build from
# the repo root:
#
#   docker build -f deploy/operator/Dockerfile -t agentctl/operator:dev .
FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN cargo build --release -p agentctl-operator --bin agentctl-operator

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /build/target/release/agentctl-operator /usr/local/bin/agentctl-operator
ENTRYPOINT ["/usr/local/bin/agentctl-operator"]
