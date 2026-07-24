param(
  [string]$Target = 'x86_64-pc-windows-msvc'
)

$ErrorActionPreference = 'Stop'

Push-Location (Split-Path $PSScriptRoot -Parent)
try {
  cargo build --release --target $Target
  if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
  }
  if ($Target -eq 'x86_64-pc-windows-msvc') {
    Copy-Item 'target/x86_64-pc-windows-msvc/release/tmux-mcp-rs.exe' 'bin/tmux-mcp-rs-windows-x64.exe' -Force
  }
} finally {
  Pop-Location
}
