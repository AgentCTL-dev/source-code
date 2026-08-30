# agentd-staging: the reference-agent binary on busybox, for the admission
# webhook's rung-4 initContainer (`cp /agentd /agentd-bin/agentd`). The
# upstream image is FROM scratch — no shell, no cp — so until upstream ask U9
# (a --copy-self helper) lands, this derived image is the staging vehicle.
ARG AGENTD_IMAGE=agentd:1.3.1
FROM ${AGENTD_IMAGE} AS agentd
FROM busybox:1.36-uclibc
COPY --from=agentd /agentd /agentd
USER 65532:65532
ENTRYPOINT ["/agentd"]
