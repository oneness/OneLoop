#!/usr/bin/env bash
set -euo pipefail

export ONELOOP_OPENROUTER_MODEL="deepseek/deepseek-v4-flash"
export ONELOOP_OPENAI_MODEL="gpt-5.5"
export ONELOOP_ANTHROPIC_MODEL="claude-opus-4-7"

export ONELOOP_ORIGINAL_DIR="$(pwd)"
cd "$(dirname "$(readlink -f "$0")")"
exec nix --quiet develop -c cargo run --quiet -- "$@"
