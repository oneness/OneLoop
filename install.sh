#!/usr/bin/env bash
set -euo pipefail

# Install the released oneloop binary, plus the `ol` alias beside it.
#
#   curl --fail --silent --show-error --location \
#     https://raw.githubusercontent.com/oneness/OneLoop/main/install.sh | sh
#
# ONELOOP_VERSION pins a release instead of taking the latest.
# ONELOOP_BIN installs somewhere other than ~/.local/bin.
#
# Only the binary is installed. Providers and models live in
# ~/.oneloop/config.json, which this script does not write: a fresh install
# starts on the local default model.

repo="oneness/OneLoop"
bin_dir="${ONELOOP_BIN:-${HOME}/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) asset="x86_64-linux" ;;
  *)
    echo "no release for $(uname -s) $(uname -m); build from source instead" >&2
    exit 1
    ;;
esac

version="${ONELOOP_VERSION:-}"
if [[ -z "$version" ]]; then
  # No releases yet, or no network: an empty version falls through to the
  # message below rather than to curl's own error.
  version="$(curl --fail --silent --show-error --location \
    "https://api.github.com/repos/${repo}/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)" || true
fi

if [[ -z "$version" ]]; then
  echo "could not resolve the latest release of ${repo}" >&2
  exit 1
fi

url="https://github.com/${repo}/releases/download/${version}/oneloop-${version}-${asset}.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

echo "downloading oneloop ${version} for ${asset}"
curl --fail --silent --show-error --location --output "${tmp}/oneloop.tar.gz" "${url}"
tar --extract --gzip --file "${tmp}/oneloop.tar.gz" --directory "${tmp}"

mkdir -p "${bin_dir}"
install -m 0755 "${tmp}/oneloop/oneloop" "${bin_dir}/oneloop"
ln --symbolic --force oneloop "${bin_dir}/ol"

echo "installed ${bin_dir}/oneloop (${version}) and ${bin_dir}/ol"
case ":${PATH}:" in
  *":${bin_dir}:"*) echo "run: ol" ;;
  *) echo "add it to PATH, then run 'ol':  export PATH=\"${bin_dir}:\$PATH\"" ;;
esac
