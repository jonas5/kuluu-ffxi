#!/usr/bin/env bash
set -euo pipefail

BACKEND="${1:-vulkan}"

case "$BACKEND" in
    dx12|vulkan|gl) ;;
    *)
        echo "Usage: $0 [vulkan|gl|dx12]"
        exit 1
        ;;
esac

if [ "$BACKEND" = dx12 ] && [ "$(uname -s)" != Windows_NT ]; then
    echo "warning: wgpu's DX12 backend only exists on Windows; falling back to vulkan" >&2
    BACKEND=vulkan
fi

export WGPU_BACKEND="$BACKEND"
export FFXI_DAT_PATH="${FFXI_DAT_PATH:-$HOME/HorizonXI/Game/SquareEnix/FINAL FANTASY XI}"
export FFXI_DIAG_STREAM=1

# OOM stopgap (kuluu-94n2): cap kuluu's cgroup so the kernel kills it cleanly
# at KULUU_MEM_MAX instead of thrashing the whole machine. Set KULUU_MEM_MAX=0
# to disable. Default derives from MemAvailable so a co-resident QEMU VM (8GB)
# can't be starved: total budget = available - 4GB desktop floor, clamped to
# [8G, 20G]. systemd-run dwarfs nothing; the scope ends when the client exits,
# so the memory is released on death either way.
MEM_MAX="${KULUU_MEM_MAX:-auto}"
if [ "$MEM_MAX" = auto ] && command -v systemd-run >/dev/null 2>&1; then
    AVAIL_KB=$(awk '/MemAvailable/ {print $2}' /proc/meminfo)
    WANT_MB=$(( (AVAIL_KB - 4 * 1024 * 1024) / 1024 ))
    if [ "$WANT_MB" -lt 8192 ]; then
        WANT_MB=8192
    elif [ "$WANT_MB" -gt 20480 ]; then
        WANT_MB=20480
    fi
    MEM_MAX="${WANT_MB}M"
fi
if [ "$MEM_MAX" != 0 ] && command -v systemd-run >/dev/null 2>&1; then
    exec systemd-run --user --scope \
        -p "MemoryMax=$MEM_MAX" -p MemorySwapMax=0 \
        ./target/release/kuluu play
fi

exec ./target/release/kuluu play
