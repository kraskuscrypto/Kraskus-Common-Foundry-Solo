#!/usr/bin/env bash
# Host entry point: builds the current commit in the pinned toolchain image.
# Usage: KRASKUS/build/build.sh <output-directory>
set -euo pipefail
root="$(git rev-parse --show-toplevel)"
out="${1:?output directory}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
image="$(grep '^TOOLCHAIN_IMAGE=' "$root/KRASKUS/build/PINS" | cut -d= -f2)"
commit="$(git -C "$root" rev-parse HEAD)"
docker run --rm \
  -v "$root:/src:ro" \
  -v "$out:/out" \
  -e CMFD_BUILD_SOURCE_COMMIT="$commit" \
  -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
  "$image" \
  bash -c 'apt-get -qq update >/dev/null && apt-get -qq install -y --no-install-recommends git python3 >/dev/null \
    && git config --global --add safe.directory /src \
    && bash /src/KRASKUS/build/build-in-container.sh'
