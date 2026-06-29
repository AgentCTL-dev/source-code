# Runtime image for the ModelGateway (RFC 0012). Distroless nonroot; serves the
# inference proxy on :8080.
#
# Self-contained multi-stage build: the release binary is compiled inside the
# image (no host-built binary required, so CI needs no prior `cargo build`).
FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN cargo build --release -p agentctl-modelgateway --bin agentctl-modelgateway

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /build/target/release/agentctl-modelgateway /usr/local/bin/agentctl-modelgateway
ENTRYPOINT ["/usr/local/bin/agentctl-modelgateway"]
