# Runtime image for the A2A gateway (RFC 0013). Distroless nonroot; serves the
# A2A HTTP surface on :8080. Build from repo root after
# `cargo build --release -p agentctl-gateway`.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY target/release/agentctl-gateway /usr/local/bin/agentctl-gateway
ENTRYPOINT ["/usr/local/bin/agentctl-gateway"]
