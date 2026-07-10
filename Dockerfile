# syntax=docker/dockerfile:1

ARG TMUX_MCP_RS_VERSION=0.5.0
ARG TMUX_MCP_RS_REPOSITORY=bnomei/tmux-mcp

# Fetch the prebuilt musl Linux release asset for the image architecture.
FROM --platform=$BUILDPLATFORM alpine:3.22 AS fetch
ARG TMUX_MCP_RS_VERSION
ARG TMUX_MCP_RS_REPOSITORY
ARG TARGETARCH

RUN apk add --no-cache ca-certificates curl

RUN set -eux; \
  case "$TARGETARCH" in \
    amd64) target=x86_64-unknown-linux-musl ;; \
    arm64) target=aarch64-unknown-linux-musl ;; \
    *) echo "unsupported Docker target architecture: $TARGETARCH" >&2; exit 1 ;; \
  esac; \
  tag="v${TMUX_MCP_RS_VERSION#v}"; \
  archive="tmux-mcp-rs-${tag}-${target}.tar.gz"; \
  url="https://github.com/${TMUX_MCP_RS_REPOSITORY}/releases/download/${tag}/${archive}"; \
  curl -fsSL -o "/tmp/${archive}" "$url"; \
  curl -fsSL -o "/tmp/${archive}.sha256" "${url}.sha256"; \
  cd /tmp; \
  sha256sum -c "${archive}.sha256"; \
  tar -xzf "$archive"; \
  chmod 755 tmux-mcp-rs; \
  mkdir -p /tmp/tmux-mcp-workspace

# Self-contained runtime: Alpine tmux 3.x + static musl binary.
# Default model: sessions live inside the container (docker exec to attach).
# Optional Linux host-socket mode still needs this client binary; see packaging/README.md.
FROM alpine:3.22
ARG TMUX_MCP_RS_VERSION

RUN apk add --no-cache ca-certificates tmux \
  && adduser -D -u 65532 -h /workspace nonroot

ENV HOME=/workspace

LABEL org.opencontainers.image.title="tmux-mcp-rs"
LABEL org.opencontainers.image.description="Tmux MCP server in Rust"
LABEL org.opencontainers.image.source="https://github.com/bnomei/tmux-mcp"
LABEL org.opencontainers.image.licenses="MIT"
LABEL org.opencontainers.image.version="${TMUX_MCP_RS_VERSION}"

COPY --from=fetch --chown=65532:65532 /tmp/tmux-mcp-rs /usr/local/bin/tmux-mcp-rs
COPY --from=fetch --chown=65532:65532 /tmp/tmux-mcp-workspace /workspace

WORKDIR /workspace
USER 65532:65532

ENTRYPOINT ["/usr/local/bin/tmux-mcp-rs"]
