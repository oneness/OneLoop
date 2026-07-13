#!/usr/bin/env bash
set -euo pipefail

# Best coding model per provider as of 2026-07-12.
# Keep in sync with the defaults in src/providers/*.rs.
export ONELOOP_OPENROUTER_MODEL="deepseek/deepseek-v4-flash"
export ONELOOP_OPENAI_MODEL="gpt-5.6-sol"
export ONELOOP_ANTHROPIC_MODEL="claude-opus-4-8"

export ONELOOP_ORIGINAL_DIR="$(pwd)"
cd "$(dirname "$(readlink -f "$0")")"

binary="./target/release/oneloop"
if [[ -x "$binary" ]] \
  && ! find src Cargo.toml Cargo.lock flake.nix flake.lock -newer "$binary" -print -quit | grep -q .; then
  exec "$binary" "$@"
fi

exec nix --quiet develop -c cargo run --quiet -- "$@"
