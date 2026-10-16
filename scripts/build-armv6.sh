#!/usr/bin/env bash
# Cross-compile for the Raspberry Pi Zero / Zero W (ARMv6, static musl).
#
#   ./scripts/build-armv6.sh              # build the whole workspace
#   ./scripts/build-armv6.sh -p fridged   # a single package
set -euo pipefail

TARGET=arm-unknown-linux-musleabihf
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

command -v cross >/dev/null || {
    echo "cross not found. Install it with:" >&2
    echo "  cargo install cross --git https://github.com/cross-rs/cross --locked" >&2
    exit 1
}
docker info >/dev/null 2>&1 || {
    echo "Docker is not running: cross needs it." >&2
    exit 1
}

# RUSTFLAGS in the environment would override (not extend) the one from
# .cargo/config.toml, losing -C target-cpu=arm1176jzf-s.
if [[ -n "${RUSTFLAGS:-}" ]]; then
    echo "WARNING: RUSTFLAGS is set ('$RUSTFLAGS') and overrides the target" >&2
    echo "         configuration. Unsetting it for this build." >&2
    unset RUSTFLAGS
fi

echo "==> cross build --release --target $TARGET $*"
cross build --release --target "$TARGET" "$@"

echo
echo "==> artifacts in target/$TARGET/release/"
find "target/$TARGET/release" -maxdepth 1 -type f -perm -u+x ! -name '*.d' -exec ls -lh {} +
echo
echo "Check ARMv6 compatibility with: ./scripts/verify-armv6.sh <binary>"
