# Runtime image for the agentctl aggregated apiserver (RFC 0009). Distroless
# nonroot; serves HTTPS on 6443 with the mounted serving cert.
#
# Self-contained multi-stage build: the release binary is compiled inside the
# image (no host-built binary required, so CI needs no prior `cargo build`).
FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN cargo build --release -p agentctl-apiserver --bin agentctl-apiserver

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /build/target/release/agentctl-apiserver /usr/local/bin/agentctl-apiserver
ENTRYPOINT ["/usr/local/bin/agentctl-apiserver"]
