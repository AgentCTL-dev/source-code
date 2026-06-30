# Delivery image for the stdio<->HTTP work.* bridge (Finding B).
#
# agentd's MCP client is stdio-only (it spawns a child and speaks NDJSON), while
# the coordination server serves work.* over HTTP JSON-RPC. agentd spawns this
# bridge as its `--mcp` child INSIDE its own (scratch) container, so the bridge
# binary must be FULLY STATIC (musl) — a glibc/distroless build would have no
# loader/libc in scratch. We therefore build it static-musl (like agentd) and
# ship it on busybox so an initContainer can `cp` it onto the shared emptyDir
# the agentd container then execs from. See e2e/manifests/claim-deployment.yaml.
#
# Build from the repo root (the bin is planned at
# crates/agentctl-coordination/src/bin/work-mcp-bridge.rs):
#   docker buildx build -f e2e/work-mcp-bridge.Dockerfile -t agentctl/work-mcp-bridge:dev .

FROM rust:1.88-alpine AS build
# Alpine's host target IS <arch>-unknown-linux-musl (crt-static on), so the
# release binary is static. musl-dev supplies the static C runtime stubs.
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
RUN cargo build --release --locked -p agentctl-coordination --bin work-mcp-bridge \
 && cp target/release/work-mcp-bridge /usr/local/bin/work-mcp-bridge

# busybox (musl, static) gives the initContainer a `cp` to stage the binary onto
# the shared volume; the binary itself runs inside agentd's scratch container.
FROM busybox:1.36
COPY --from=build /usr/local/bin/work-mcp-bridge /usr/local/bin/work-mcp-bridge
ENTRYPOINT ["/usr/local/bin/work-mcp-bridge"]
