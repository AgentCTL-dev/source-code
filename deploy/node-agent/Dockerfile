# Runtime image for the node-agent (RFC 0008, Tier A). Host-side bridge: runs as
# root to read per-pod management sockets on the shared hostPath.
#
# Self-contained multi-stage build: the release binary is compiled inside the
# image (no host-built binary required, so CI needs no prior `cargo build`).
# Note the crate is `agentctl-node-agent` but its binary is named `node-agent`.
FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN cargo build --release -p agentctl-node-agent --bin node-agent

FROM gcr.io/distroless/cc-debian12
COPY --from=build /build/target/release/node-agent /usr/local/bin/node-agent
ENTRYPOINT ["/usr/local/bin/node-agent"]
