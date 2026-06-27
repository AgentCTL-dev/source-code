# Runtime image for the agentctl aggregated apiserver (RFC 0009). Distroless
# nonroot; serves HTTPS on 6443 with the mounted serving cert. Build from repo
# root after `cargo build --release -p agentctl-apiserver`.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY target/release/agentctl-apiserver /usr/local/bin/agentctl-apiserver
ENTRYPOINT ["/usr/local/bin/agentctl-apiserver"]
