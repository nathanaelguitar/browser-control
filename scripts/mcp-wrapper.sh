#!/usr/bin/env bash
# Self-bootstrapping launcher for the browser-control MCP server, invoked by
# Canopy Code via canopy-extension.json.
#
# First run on a machine: installs Rust (via rustup) if missing, then builds
# the release binary from this checked-out source. Every run after that just
# execs the already-built binary — cheap, no network, no rebuild.
#
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$DIR/target/release/browser-control"

if [ ! -x "$BIN" ]; then
  echo "browser-control: binary not found, bootstrapping (one-time setup)..." >&2

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

  echo "browser-control: building release binary (this can take a minute)..." >&2
  ( cd "$DIR" && cargo build --release --locked ) >&2

  if [ ! -x "$BIN" ]; then
    echo "browser-control: error: build did not produce $BIN" >&2
    exit 1
  fi
  echo "browser-control: bootstrap complete." >&2
fi

exec "$BIN" mcp
