#!/bin/sh
# CI guard for build inputs that must not float:
#  - every workflow action is pinned to a full commit SHA (a moved tag can
#    change what CI runs without a commit here);
#  - the release image builds with the same Rust as rust-toolchain.toml
#    (a floating `stable` broke clippy on every PR on 2026-11-14);
#  - the Cello runtime is pinned by tag + digest (a floating tag would
#    change what the release image runs without a commit here).
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

#  - the Cello runtime is pinned by tag + digest, and the image ref is built
#    from exactly those two values (a floating tag would change what the
#    release image runs without a commit here).
version=$(sed -n 's/^ARG CELLO_VERSION=\(.*\)$/\1/p' docker/Dockerfile)
digest=$(sed -n 's/^ARG CELLO_DIGEST=\(.*\)$/\1/p' docker/Dockerfile)
imageref=$(sed -n 's|^ARG CELLO_IMAGE=\(.*\)$|\1|p' docker/Dockerfile)
case "$version" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "docker/Dockerfile CELLO_VERSION '$version' is not a semver pin" >&2; status=1 ;;
esac
case "$digest" in
  sha256:????????????????????????????????????????????????????????????????) ;;
  *) echo "docker/Dockerfile CELLO_DIGEST '$digest' is not a sha256 pin" >&2; status=1 ;;
esac
# The Dockerfile holds the unexpanded form literally.
# shellcheck disable=SC2016
if [ "$imageref" != 'ghcr.io/ryuzcorp/cello:${CELLO_VERSION}@${CELLO_DIGEST}' ]; then
  echo "docker/Dockerfile CELLO_IMAGE '$imageref' is not built from the pinned version and digest" >&2
  status=1
fi

exit "$status"
