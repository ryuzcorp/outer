#!/bin/sh
# CI guard for build inputs that must not float:
#  - every workflow action is pinned to a full commit SHA (a moved tag can
#    change what CI runs without a commit here);
#  - the release image builds with the same Rust as rust-toolchain.toml
#    (a floating `stable` broke clippy on every PR on 2026-11-14).
set -eu
cd "$(dirname "$0")/.."

status=0

unpinned=$(grep -nE '^\s*-?\s*uses:' .github/workflows/*.yml | grep -vE 'uses:\s*[^@[:space:]]+@[0-9a-f]{40}(\s|$)' || true)
if [ -n "$unpinned" ]; then
  echo "actions not pinned to a commit SHA:" >&2
  echo "$unpinned" >&2
  status=1
fi

channel=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
image=$(sed -n 's|^FROM docker.io/library/rust:\([^ ]*\)-bookworm AS build$|\1|p' docker/Dockerfile)
if [ -z "$channel" ] || [ "$channel" != "$image" ]; then
  echo "rust-toolchain.toml channel '$channel' and docker/Dockerfile rust image '$image' differ" >&2
  status=1
fi

exit "$status"
