# The AAuth-verifying remote-MCP fixture (RFC 0024 e2e phase 0): verifies
# RFC 9421 signatures + aa-agent+jwt agent tokens against the e2e Agent
# Provider's JWKS, serves a minimal MCP handshake, and exposes /stats.
#
# Self-building (compile in-image) so it does NOT depend on the host `target/`
# (which .dockerignore excludes) — build from the repo root:
#   docker build -t mock-aauth-mcp:dev -f deploy/examples/mock-aauth-mcp.Dockerfile .
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p mock-agent --bin mock-aauth-mcp

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /src/target/release/mock-aauth-mcp /usr/local/bin/mock-aauth-mcp
ENTRYPOINT ["/usr/local/bin/mock-aauth-mcp"]
