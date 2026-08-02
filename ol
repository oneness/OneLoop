#!/usr/bin/env bash
set -euo pipefail

# Endpoints (which URL, which model) live in ~/.oneloop/endpoints.json, and
# the default is the local server. Nothing to export here: setting
# ONELOOP_OPENROUTER_MODEL would override the default endpoint's model and
# quietly defeat that.
#
# To use a hosted model for one run:  ONELOOP_PROVIDER=openrouter ol "..."

export ONELOOP_ORIGINAL_DIR="$(pwd)"
cd "$(dirname "$(readlink -f "$0")")"

binary="./target/release/oneloop"
if [[ -x "$binary" ]] \
  && ! find src Cargo.toml Cargo.lock flake.nix flake.lock -newer "$binary" -print -quit | grep -q .; then
  exec "$binary" "$@"
fi

exec nix --quiet develop -c cargo run --quiet -- "$@"
