#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"
export ONELOOP_QUIET=1
exec nix --quiet develop -c cargo run --quiet -- "$@"
