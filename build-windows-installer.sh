#!/usr/bin/env bash
set -euo pipefail

# Cross-compile OpenSD3 (VST3 + CLAP + Standalone + sd3tool CLI) for 64-bit Windows
# and package into a single graphical NSIS installer (.exe).
#
# Prerequisites:
#   - Arch Linux : sudo pacman -S --needed rust mingw-w64-gcc nsis && rustup target add x86_64-pc-windows-gnu
#   - Debian/Ubuntu: sudo apt install gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 nsis && rustup target add x86_64-pc-windows-gnu

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

VERSION="${APP_VERSION:-0.3.0}"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
OUT_EXE="${1:-$ROOT_DIR/OpenSD3-${VERSION}-Windows-x64-Setup.exe}"
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/opensd3_win_stage.XXXXXX")"
trap 'rm -rf "$STAGE_DIR"' EXIT

echo "==> Cross-compiling VST3 & CLAP bundles for x86_64-pc-windows-gnu..."
cargo run --package xtask -- bundle open-sd3 --release --target x86_64-pc-windows-gnu

echo "==> Cross-compiling open-sd3.exe (Standalone GUI) & sd3tool.exe (CLI)..."
cargo build --release --target x86_64-pc-windows-gnu -p sd3-cli -p open-sd3 --bins

echo "==> Staging & stripping debug symbols..."
mkdir -p "$STAGE_DIR/vst3/open-sd3.vst3/Contents/x86_64-win" "$STAGE_DIR/clap" "$STAGE_DIR/app"
cp "$TARGET_DIR/bundled/open-sd3.vst3/Contents/x86_64-win/open-sd3.vst3" \
   "$STAGE_DIR/vst3/open-sd3.vst3/Contents/x86_64-win/open-sd3.vst3"
cp "$TARGET_DIR/bundled/open-sd3.clap" "$STAGE_DIR/clap/open-sd3.clap"
cp "$TARGET_DIR/x86_64-pc-windows-gnu/release/open-sd3.exe" "$STAGE_DIR/app/open-sd3.exe"
cp "$TARGET_DIR/x86_64-pc-windows-gnu/release/sd3tool.exe" "$STAGE_DIR/app/sd3tool.exe"
cp "$ROOT_DIR/COPYING" "$STAGE_DIR/COPYING.txt"
cp "$ROOT_DIR/README.md" "$STAGE_DIR/README.md"
cp "$ROOT_DIR/docs/SUPERIOR_DRUMMER_3_FORMAT_SPEC.md" "$STAGE_DIR/SUPERIOR_DRUMMER_3_FORMAT_SPEC.md"

if command -v x86_64-w64-mingw32-strip >/dev/null 2>&1; then
    x86_64-w64-mingw32-strip --strip-debug \
        "$STAGE_DIR/vst3/open-sd3.vst3/Contents/x86_64-win/open-sd3.vst3" \
        "$STAGE_DIR/clap/open-sd3.clap" \
        "$STAGE_DIR/app/open-sd3.exe" \
        "$STAGE_DIR/app/sd3tool.exe"
fi

echo "==> Building NSIS Windows x64 installer: $OUT_EXE"
makensis \
    -DAPP_VERSION="$VERSION" \
    -DSTAGE_DIR="$STAGE_DIR" \
    -DOUT_FILE="$OUT_EXE" \
    "$ROOT_DIR/installer/windows/opensd3.nsi"

ls -lh "$OUT_EXE"
