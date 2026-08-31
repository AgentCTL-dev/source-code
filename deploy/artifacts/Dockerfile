# The artifacts façade (P3-3): the artifacts.* MCP backend over an S3-compatible
# content store (MinIO by default), org-fenced with per-org quotas. See the
# annotated copy at deploy/operator/Dockerfile; multi-arch via cargo-zigbuild.
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p agentctl-artifacts

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/agentctl-artifacts /usr/local/bin/agentctl-artifacts
ENTRYPOINT ["/usr/local/bin/agentctl-artifacts"]
