# Runtime image for the validating admission webhook (RFC 0007). Distroless
# nonroot; serves HTTPS on :8443.
#
# Self-contained multi-stage build: the release binary is compiled inside the
# image (no host-built binary required, so CI needs no prior `cargo build`).
FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN cargo build --release -p agentctl-admission --bin agentctl-admission

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /build/target/release/agentctl-admission /usr/local/bin/agentctl-admission
ENTRYPOINT ["/usr/local/bin/agentctl-admission"]
