# The sandbox cell (P5-5): the sandbox.* MCP backend that runs untrusted code
# in single-use, network-denied, capability-stripped pods. See the annotated
# copy at deploy/operator/Dockerfile; multi-arch via cargo-zigbuild.
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p agentctl-sandbox

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/agentctl-sandbox /usr/local/bin/agentctl-sandbox
ENTRYPOINT ["/usr/local/bin/agentctl-sandbox"]
