# Runtime image for the A2A gateway (RFC 0013). Distroless nonroot; serves the
# A2A HTTP surface on :8080.
#
# Self-contained multi-stage build: the release binary is compiled inside the
# image (no host-built binary required, so CI needs no prior `cargo build`).
FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN cargo build --release -p agentctl-gateway --bin agentctl-gateway

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /build/target/release/agentctl-gateway /usr/local/bin/agentctl-gateway
ENTRYPOINT ["/usr/local/bin/agentctl-gateway"]
