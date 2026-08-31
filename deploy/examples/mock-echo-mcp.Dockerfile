# The credential-injection WITNESS (P5-3 e2e): a plain-HTTP MCP fixture whose
# one tool `auth.echo` returns the Authorization header the call arrived with
# — proving which credential the tenant gateway injected upstream.
#
# Self-building (compile in-image) so it does NOT depend on the host `target/`
# (which .dockerignore excludes) — build from the repo root:
#   docker build -t mock-echo-mcp:dev -f deploy/examples/mock-echo-mcp.Dockerfile .
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p mock-agent --bin mock-echo-mcp

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /src/target/release/mock-echo-mcp /usr/local/bin/mock-echo-mcp
ENTRYPOINT ["/usr/local/bin/mock-echo-mcp"]
