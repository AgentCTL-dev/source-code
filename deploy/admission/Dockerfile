# Runtime image for the validating admission webhook (RFC 0007). Distroless
# nonroot; serves HTTPS on :8443. Build from repo root after
# `cargo build --release -p agentctl-admission`.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY target/release/agentctl-admission /usr/local/bin/agentctl-admission
ENTRYPOINT ["/usr/local/bin/agentctl-admission"]
