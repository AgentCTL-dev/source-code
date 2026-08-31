# The audit shipper (P7-3): tails a tenant gateway's hash-chained audit file
# and ships audit/v1 batches to identity's ingest door. Build from repo root:
#   docker build -t agentctl/audit-shipper -f deploy/audit-shipper/Dockerfile .
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p agentctl-audit --bin agentctl-audit-shipper

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/agentctl-audit-shipper /usr/local/bin/agentctl-audit-shipper
ENTRYPOINT ["/usr/local/bin/agentctl-audit-shipper"]
