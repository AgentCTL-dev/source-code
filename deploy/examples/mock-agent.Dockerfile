# A conformant-agent stand-in (serves the management profile only) for e2e.
# Runs as root so it can bind its socket in the kubelet-created hostPath subdir.
# Build from repo root after `cargo build --release -p mock-agent`.
FROM gcr.io/distroless/cc-debian12
COPY target/release/mock-agent /usr/local/bin/mock-agent
ENTRYPOINT ["/usr/local/bin/mock-agent"]
