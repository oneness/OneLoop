#!/usr/bin/env bash
set -euo pipefail

export ONELOOP_ORIGINAL_DIR="$(pwd)"
cd "$(dirname "$(readlink -f "$0")")"
exec nix --quiet develop -c cargo run --quiet -- "$@"
