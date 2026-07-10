# tmux-mcp-rs Packaging

GitHub Release assets are the source of truth for binary installers:

- `npx @bnomei/tmux-mcp-rs` is published from `npm/tmux-mcp-rs`; the wrapper downloads the matching GitHub Release asset and verifies its `.sha256`.
- `docker run ghcr.io/bnomei/tmux-mcp:<version>` is built from `Dockerfile`, which downloads the musl Linux release asset for the target image architecture and runs on Alpine with `tmux` installed.
- Homebrew formula metadata lives in the sibling `homebrew-tmux-mcp` repository and consumes the macOS/Linux `.tar.gz` assets plus published `.sha256` checksums.

The npm publish job is skipped unless the release environment provides `NPM_TOKEN`.

## Release asset matrix

Default release workflow targets:

| Target | Notes |
| --- | --- |
| `x86_64-unknown-linux-musl` | Used by Docker (amd64) and the Linux x64 npm wrapper |
| `aarch64-unknown-linux-musl` | Used by Docker (arm64) and the Linux arm64 npm wrapper |
| `x86_64-apple-darwin` | Intel macOS npm / Homebrew |
| `aarch64-apple-darwin` | Apple Silicon npm / Homebrew |
| `x86_64-pc-windows-msvc` | Windows `.zip` for the npm wrapper |

Local packaging smoke:

```bash
VERSION=0.5.0 TARGET=x86_64-unknown-linux-musl scripts/build-release.sh
VERSION=0.5.0 TARGET=x86_64-unknown-linux-musl scripts/package-release.sh

VERSION=0.5.0 TARGET=aarch64-unknown-linux-musl scripts/build-release.sh
VERSION=0.5.0 TARGET=aarch64-unknown-linux-musl scripts/package-release.sh
```

Use `cross` for musl targets unless the matching musl C toolchain is installed on the host.

## npm wrapper

Package directory: `npm/tmux-mcp-rs`

```bash
# after a GitHub Release exists for this version
cd npm/tmux-mcp-rs
npm version 0.5.0 --no-git-tag-version --allow-same-version
npm publish --access public
```

Override / debug env vars:

| Variable | Purpose |
| --- | --- |
| `TMUX_MCP_RS_VERSION` | Pin a release tag/version (default: package.json version) |
| `TMUX_MCP_RS_REPOSITORY` | Override `owner/repo` for download URLs |
| `TMUX_MCP_RS_NPM_CACHE` | Binary cache directory |
| `TMUX_MCP_RS_LOCAL_BIN` | Use a local binary path instead of downloading |
| `TMUX_MCP_RS_SKIP_DOWNLOAD` | Fail if the binary is not already cached |

## Docker image

The image downloads musl Linux release assets so the container does not need a Rust toolchain. Alpine provides **tmux 3.x** at runtime (required by the server).

Published tags (from the release workflow):

- `ghcr.io/bnomei/tmux-mcp:<version>`
- `ghcr.io/bnomei/tmux-mcp:latest`

### Mode A — self-contained (default)

This is the intended Docker packaging model:

| Piece | Where it runs |
| --- | --- |
| `tmux-mcp-rs` MCP server | container |
| `tmux` server + sessions | **inside** the container |
| Human attach | `docker exec -it <container> tmux attach …` |

Build and smoke (after GitHub Release assets for that version exist):

```bash
docker build \
  --build-arg TMUX_MCP_RS_VERSION=0.5.0 \
  -t tmux-mcp-rs:0.5.0 .

docker run --rm tmux-mcp-rs:0.5.0 --version
docker run --rm tmux-mcp-rs:0.5.0 --help
```

stdio MCP client (keep stdin attached; mount a workspace if agents need files):

```bash
docker run --rm -i \
  -v "$PWD:/workspace" \
  --name tmux-mcp \
  ghcr.io/bnomei/tmux-mcp:0.5.0
```

Attach from another terminal while that container is running:

```bash
docker exec -it tmux-mcp tmux attach -t workspace
# or list: docker exec -it tmux-mcp tmux list-sessions
```

Implications:

- Sessions die when the container exits unless you deliberately persist tmux state (not the default path).
- Good for CI, sandboxes, and multi-arch GHCR distribution.
- Does **not** drive your host desktop tmux automatically.

### Mode B — shared host socket (Linux advanced)

Use this only when a **host** tmux server must be the session owner (native `tmux attach` on the host). The container still needs its own `tmux` **client** binary to speak the protocol over the mounted socket.

| Piece | Where it runs |
| --- | --- |
| `tmux-mcp-rs` MCP server | container |
| `tmux` server + sessions | **host** |
| Human attach | host `tmux -S … attach` |

```bash
# host
tmux -S /tmp/tmux-mcp-agent.sock -f /dev/null new-session -d -s workspace

# container (Linux Docker engine with host socket mount)
docker run --rm -i \
  -v /tmp/tmux-mcp-agent.sock:/tmp/tmux-mcp-agent.sock \
  --user "$(id -u):$(id -g)" \
  ghcr.io/bnomei/tmux-mcp:0.5.0 \
  --socket /tmp/tmux-mcp-agent.sock

# host
tmux -S /tmp/tmux-mcp-agent.sock attach -t workspace
```

Constraints:

- **Linux Docker only** in practice. Docker Desktop on macOS/Windows runs a VM and typically cannot share the host’s Unix domain sockets for tmux.
- Socket path and **UID/GID** must match; the default image user is `65532:65532`, so override with `--user` when talking to a host-owned socket.
- Host tmux must be **3.0+** (same version gate as a native install).
- Prefer native Homebrew / cargo / npm installs when the main goal is co-attaching on a developer laptop.

### What we deliberately do not do

- Ship a binary-only image that assumes host `tmux` is always present.
- Treat “connect to my laptop’s default tmux” as the default Docker behavior.
