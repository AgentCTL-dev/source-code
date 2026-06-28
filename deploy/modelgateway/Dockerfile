# Runtime image for the ModelGateway (RFC 0012). Distroless nonroot; serves the
# inference proxy on :8080. Build from repo root after
# `cargo build --release -p agentctl-modelgateway`.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY target/release/agentctl-modelgateway /usr/local/bin/agentctl-modelgateway
ENTRYPOINT ["/usr/local/bin/agentctl-modelgateway"]
