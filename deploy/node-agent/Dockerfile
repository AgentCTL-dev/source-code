# Runtime image for the node-agent (RFC 0008, Tier A). Host-side bridge: runs as
# root to read per-pod management sockets on the shared hostPath. Build from repo
# root after `cargo build --release -p agentctl-node-agent`.
FROM gcr.io/distroless/cc-debian12
COPY target/release/node-agent /usr/local/bin/node-agent
ENTRYPOINT ["/usr/local/bin/node-agent"]
