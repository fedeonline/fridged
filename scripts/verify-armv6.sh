#!/usr/bin/env bash
# Verify that a cross-compiled binary really runs on an ARM1176JZF-S (Pi Zero).
#
#   ./scripts/verify-armv6.sh target/arm-unknown-linux-musleabihf/release/spike
#
# WHY THIS EXISTS
# The `arm-unknown-linux-musleabihf` Rust target is ARMv6, but dependencies
# carrying C/assembly code (chiefly `ring`, used by rustls) can introduce ARMv7
# instructions. The ELF Tag_CPU_arch attribute is NOT a usable verdict: it reads
# "v7" because it is merged down with `ring`'s objects, even when no v7
# instruction is actually reachable.
#
# TWO CHECKS, WITH DIFFERENT WEIGHT
#   1. runtime (BLOCKING) — the binary runs under QEMU with an arm1176 CPU.
#      If an unsupported instruction were executed, QEMU would raise SIGILL.
#   2. static — scans the disassembly for post-ARMv6 instructions. BLOCKING only
#      on NON-stripped binaries, and here is why: `ring`'s OpenSSL-derived
#      assembly keeps constant tables inside .text (e.g. the SHA-512 K values).
#      On ARM these are marked by `$d` mapping symbols, which objdump uses to
#      avoid disassembling them as code. `strip` removes those symbols, and then
#      data gets decoded as instructions: the SHA-512 constant
#      0x4ed8aa4ae3418acb, for instance, "appears" as a movt. On a stripped
#      binary this check is therefore informational only.
#
# CAVEAT: QEMU enforces the ISA less strictly than real hardware, and it only
# exercises the paths the program actually takes. The definitive verdict is
# still running on the Pi (task 0.5 / Gate 0).
#
# Re-run after every dependency or cross-image update.
set -euo pipefail

BIN="${1:-}"
[[ -n "$BIN" && -f "$BIN" ]] || { echo "usage: $0 <arm-binary>" >&2; exit 2; }

BIN_ABS="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
IMAGE=fridged-armv6-check:1

docker info >/dev/null 2>&1 || { echo "Docker is not running." >&2; exit 1; }

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "==> building verification image $IMAGE (first time only)"
    docker build -q -t "$IMAGE" - <<'DOCKERFILE' >/dev/null
FROM debian:bookworm-slim
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends \
      binutils-arm-linux-gnueabihf qemu-user-static file \
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE
fi

docker run --rm -v "$BIN_ABS:/bin-under-test:ro" "$IMAGE" bash -euo pipefail -c '
B=/bin-under-test

echo "=== binary ==="
file -b "$B"
echo

#--------------------------------------------------------------------------
# 1. RUNTIME CHECK (blocking)
#--------------------------------------------------------------------------
echo "=== runtime: QEMU -cpu arm1176 (BLOCKING) ==="
FAIL=0
set +e
qemu-arm-static -cpu arm1176 "$B"
rc=$?
set -e
echo "--- (exit code: $rc)"
if [ $rc -ge 128 ]; then
    sig=$((rc-128))
    if [ $sig -eq 4 ]; then
        echo "  -> FAILED: SIGILL, instruction not supported by the ARM1176"
    else
        echo "  -> FAILED: killed by signal $sig"
    fi
    FAIL=1
else
    echo "  -> no SIGILL (a non-zero code here is an application failure:"
    echo "     off the Pi, GPIO and I2C always fail)"
fi

#--------------------------------------------------------------------------
# 2. STATIC CHECK
#--------------------------------------------------------------------------
echo
# With mapping symbols present, objdump can already tell data from code,
# so the check becomes trustworthy and therefore blocking.
if file -b "$B" | grep -q "not stripped"; then
    STRIPPED=0
    echo "=== static: post-ARMv6 instructions (BLOCKING, binary not stripped) ==="
else
    STRIPPED=1
    echo "=== static: post-ARMv6 instructions (informational, binary stripped) ==="
fi

arm-linux-gnueabihf-objdump -d "$B" > /tmp/dis.txt 2>/dev/null || {
    echo "  cannot disassemble"; exit 1; }

# NB: `grep -E` does not interpret \t as a tab. Use [[:space:]], otherwise the
# counts always come back 0 and the check "passes" without looking at anything.
POST_V6="movw|movt|sdiv|udiv"
NEON_RE="vld[1-4]\.|vst[1-4]\.|vmull\.|vshr\.|vsri\.|vrev[0-9]|vtbl\."

N_POST=$(grep -cE "[[:space:]]($POST_V6)[[:space:]]" /tmp/dis.txt || true)
N_NEON=$(grep -cE "[[:space:]]($NEON_RE)" /tmp/dis.txt || true)

printf "  movw/movt/sdiv/udiv : %s\n" "$N_POST"
printf "  NEON                : %s  (>0 expected: ring picks NEON paths at runtime via AT_HWCAP)\n" "$N_NEON"

if [ "$N_POST" -gt 0 ]; then
    echo
    echo "  occurrences, by enclosing symbol:"
    awk -v re="[[:space:]]($POST_V6)[[:space:]]" \
        "/^[0-9a-f]+ </ {fn=\$2} \$0 ~ re {print \"    \" fn}" /tmp/dis.txt \
        | sort | uniq -c | sort -rn | head -20
    if [ "$STRIPPED" -eq 0 ]; then
        echo "  -> FAILED: reachable post-ARMv6 instructions"
        FAIL=1
    else
        echo
        echo "  Stripped binary: almost certainly false positives on constant"
        echo "  tables. For a trustworthy verdict, rebuild without strip:"
        echo "    cross build --release --target arm-unknown-linux-musleabihf \\"
        echo "        --config profile.release.strip=false"
    fi
elif [ "$STRIPPED" -eq 1 ]; then
    echo
    echo "  (stripped binary: no occurrences is not a guarantee)"
fi

echo
if [ "$FAIL" -eq 0 ]; then
    echo "RESULT: no ISA problem detected under QEMU/arm1176."
    echo "        The definitive verdict is still the test on a real Pi (Gate 0)."
else
    echo "RESULT: NOT ARMv6 compatible"; exit 1
fi
'
