# Release Guide

This repo publishes the CLI in these places:

- GitHub Releases (prebuilt binaries + checksums) — source of truth
- crates.io (`cargo install` / `cargo publish`)
- npm (`npx @bnomei/tmux-mcp-rs` wrapper that downloads the matching release asset)
- GHCR Docker image (`ghcr.io/bnomei/tmux-mcp`)
- Homebrew (tap formula in `bnomei/homebrew-tmux-mcp`)

GitHub Release assets feed npm, Docker, and Homebrew, so tag/build first.

## Before You Tag

1. Update `Cargo.toml` `[package].version` (and optionally keep `npm/tmux-mcp-rs/package.json` version in sync for local docs; the release workflow rewrites npm version from the tag at publish time).
2. Optional: update `CHANGELOG.md` / README notes.
3. Run tests if needed:
   ```bash
   cargo test --lib
   cargo test --test cli
   ```

## Release (Every Time)

1. Commit and push:
   ```bash
   git add -A
   git commit -m "Release vX.Y.Z"
   git push
   ```

2. Create and push a tag (triggers the `Release` workflow):
   ```bash
   git tag vX.Y.Z
   git push --tags
   ```

   Or use workflow_dispatch with input `tag=vX.Y.Z`.

3. Wait for GitHub Actions `Release` to finish. It:
   - Builds:
     - `x86_64-unknown-linux-musl`
     - `aarch64-unknown-linux-musl`
     - `x86_64-apple-darwin`
     - `aarch64-apple-darwin`
     - `x86_64-pc-windows-msvc`
   - Uploads assets like:
     - `tmux-mcp-rs-vX.Y.Z-<target>.tar.gz` (+ `.sha256`) on Unix
     - `tmux-mcp-rs-vX.Y.Z-<target>.zip` (+ `.sha256`) on Windows
   - Builds/pushes multi-arch Docker image to `ghcr.io/bnomei/tmux-mcp`
   - Publishes `@bnomei/tmux-mcp-rs` to npm when `NPM_TOKEN` is set

4. Verify the GitHub Release has all assets and that the GHCR tags exist.

## Publish to crates.io

```bash
cargo login <CRATES_IO_TOKEN>
cargo publish
```

(`release-prepare.yml` can open a release-plz PR when configured with `CARGO_REGISTRY_TOKEN`.)

## Publish to npm (manual fallback)

Normally the release workflow publishes when `NPM_TOKEN` is configured.

```bash
cd npm/tmux-mcp-rs
npm login
npm version X.Y.Z --no-git-tag-version --allow-same-version
npm publish --access public
```

## Docker (manual fallback)

After release assets exist:

```bash
docker build --build-arg TMUX_MCP_RS_VERSION=X.Y.Z -t ghcr.io/bnomei/tmux-mcp:X.Y.Z .
docker run --rm ghcr.io/bnomei/tmux-mcp:X.Y.Z --version
```

## Publish to Homebrew (tap)

1. Tap repo: `bnomei/homebrew-tmux-mcp`, formula `Formula/tmux-mcp-rs.rb`.
2. Set `version` to `X.Y.Z` and update each `sha256` from the GitHub Release `.sha256` assets.
3. Commit and push the tap.
4. Optional:
   ```bash
   brew install bnomei/tmux-mcp/tmux-mcp-rs
   brew test bnomei/tmux-mcp/tmux-mcp-rs
   ```

## First Release Checklist

- Confirm GitHub Releases land on `bnomei/tmux-mcp`.
- Ensure npm name `@bnomei/tmux-mcp-rs` is available; set repo secret `NPM_TOKEN`.
- Ensure crates.io name `tmux-mcp-rs` is available.
- Enable GHCR package visibility as needed for `ghcr.io/bnomei/tmux-mcp`.
- Create Homebrew tap `bnomei/homebrew-tmux-mcp` with `Formula/tmux-mcp-rs.rb`.

## Notes

- npm installs download binaries from GitHub Releases based on package version (checksum verified).
- Docker downloads the musl Linux assets into an Alpine image that includes `tmux` 3.x (self-contained sessions by default; optional Linux host-socket wiring is documented in `packaging/README.md`).
- Homebrew installs use the GitHub Release tarballs + checksums from the tap formula.
- npm wrapper debug/overrides:
  - `TMUX_MCP_RS_LOCAL_BIN=/path/to/tmux-mcp-rs`
  - `TMUX_MCP_RS_SKIP_DOWNLOAD=1`
  - `TMUX_MCP_RS_VERSION`, `TMUX_MCP_RS_REPOSITORY`, `TMUX_MCP_RS_NPM_CACHE`
- Tag form is `vX.Y.Z`; the version must match `Cargo.toml`.

See also [packaging/README.md](../packaging/README.md).
