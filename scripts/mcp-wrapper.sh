#!/usr/bin/env bash
# Self-bootstrapping launcher for the browser-control MCP server, invoked by
# Canopy Code via canopy-extension.json.
#
# Ensures the release binary matches this checked-out source before starting
# the MCP server. Cargo's fingerprint check makes the no-change path cheap,
# while a source update cannot leave Canopy running an older executable.
#
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$DIR/target/release/browser-control"

if ! command -v cargo >/dev/null 2>&1; then
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "browser-control: installing Rust toolchain via rustup..." >&2
  if ! command -v curl >/dev/null 2>&1; then
    echo "browser-control: error: curl is required to install Rust; install curl and retry." >&2
    exit 1
  fi
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal >&2
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "browser-control: error: cargo still not on PATH after rustup install." >&2
  exit 1
fi

if [ ! -x "$BIN" ]; then
  echo "browser-control: binary not found, bootstrapping..." >&2
fi

( cd "$DIR" && cargo build --release --locked ) >&2

if [ ! -x "$BIN" ]; then
  echo "browser-control: error: build did not produce $BIN" >&2
  exit 1
fi

exec "$BIN" mcp
