# Runtime image for the agentctl operator.
#
# The release binary is built on the host (`cargo build --release -p
# agentctl-operator`) and copied into distroless/cc (glibc + ca-certificates,
# runs as nonroot uid 65532). The binary uses rustls (no OpenSSL), so cc is
# sufficient. Build from the repo root:
#
#   cargo build --release -p agentctl-operator --bin agentctl-operator
#   docker build -f deploy/operator/Dockerfile -t agentctl/operator:dev .
#
# (A self-contained multi-stage `FROM rust:1.95 AS build` variant can replace the
# host-build step for CI; kept host-built here for fast local iteration.)
FROM gcr.io/distroless/cc-debian12:nonroot
COPY target/release/agentctl-operator /usr/local/bin/agentctl-operator
ENTRYPOINT ["/usr/local/bin/agentctl-operator"]
