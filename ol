#!/usr/bin/env bash
set -euo pipefail

# Providers and the models they host live in ~/.oneloop/config.json, and the
# default is the local server. Nothing to export here — leaving the default
# alone is what keeps a plain run local.
#
# To use a hosted model for one run:  ONELOOP_MODEL=flash ol "..."

export ONELOOP_ORIGINAL_DIR="$(pwd)"
cd "$(dirname "$(readlink -f "$0")")"

binary="./target/release/oneloop"
if [[ -x "$binary" ]] \
  && ! find src Cargo.toml Cargo.lock flake.nix flake.lock -newer "$binary" -print -quit | grep -q .; then
  exec "$binary" "$@"
fi

exec nix --quiet develop -c cargo run --quiet -- "$@"
