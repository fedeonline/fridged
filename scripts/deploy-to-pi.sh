#!/usr/bin/env bash
# Copy a cross-compiled binary to the Pi and run it.
# In Phase 0 this is how Gate 0 (task 0.5) gets closed, using the spike.
#
#   PI_HOST=fridge@fridge.local ./scripts/deploy-to-pi.sh spike
#
# In Phase 7 the real installation will go through deploy/install.sh.
set -euo pipefail

BINARY="${1:-spike}"
PI_HOST="${PI_HOST:-}"
TARGET=arm-unknown-linux-musleabihf
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

[[ -n "$PI_HOST" ]] || {
    echo "Set PI_HOST, e.g.: PI_HOST=fridge@fridge.local $0 $BINARY" >&2
    exit 2
}

SRC="$REPO_ROOT/target/$TARGET/release/$BINARY"
[[ -f "$SRC" ]] || {
    echo "$SRC does not exist. Build it first with ./scripts/build-armv6.sh" >&2
    exit 1
}

echo "==> Pi model"
ssh "$PI_HOST" 'cat /proc/device-tree/model 2>/dev/null | tr -d "\0"; echo; grep -m1 "model name\|Hardware\|Revision" /proc/cpuinfo || true'

echo "==> copying $BINARY ($(du -h "$SRC" | cut -f1))"
scp -q "$SRC" "$PI_HOST:/tmp/$BINARY"
ssh "$PI_HOST" "chmod +x /tmp/$BINARY"

echo "==> running /tmp/$BINARY on the Pi"
# Reaching I2C and GPIO requires membership of the i2c/gpio groups, which the
# `fridge` login user already has on this Pi.
ssh "$PI_HOST" "/tmp/$BINARY"
