#!/usr/bin/env bash
set -euo pipefail

# Build and install OpenSD3 (VST3 + CLAP + Standalone + sd3tool CLI) on Arch Linux
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

echo "==> Checking Rust & Arch Linux build prerequisites..."
if ! command -v cargo >/dev/null 2>&1; then
    echo "error: 'cargo' not found. Install Rust on Arch first:"
    echo "  sudo pacman -S --needed rust base-devel pkgconf alsa-lib jack2 libx11 libxcb xcb-util-wm libxkbcommon-x11 libxcursor mesa"
    exit 1
fi

echo "==> Running workspace test suite..."
cargo test --workspace

echo "==> Bundling VST3 & CLAP plugins (--release)..."
cargo run --package xtask -- bundle open-sd3 --release

echo "==> Building sd3tool CLI & open-sd3 standalone (--release)..."
cargo build --release -p sd3-cli -p open-sd3 --bins

VST3_DIR="${HOME}/.vst3"
CLAP_DIR="${HOME}/.clap"
BIN_DIR="${HOME}/.local/bin"

mkdir -p "$VST3_DIR" "$CLAP_DIR" "$BIN_DIR"
rm -rf "${VST3_DIR}/open-sd3.vst3"
cp -r target/bundled/open-sd3.vst3 "${VST3_DIR}/"
cp -f target/bundled/open-sd3.clap "${CLAP_DIR}/"
install -m 755 target/release/sd3tool "${BIN_DIR}/sd3tool"
install -m 755 target/release/open-sd3 "${BIN_DIR}/open-sd3"

echo ""
echo "==> Installed successfully:"
echo "  VST3 plugin : ${VST3_DIR}/open-sd3.vst3"
echo "  CLAP plugin : ${CLAP_DIR}/open-sd3.clap"
echo "  Standalone  : ${BIN_DIR}/open-sd3"
echo "  CLI tool    : ${BIN_DIR}/sd3tool"
echo ""
echo "Rescan VST3/CLAP plugins in REAPER / Bitwig / Ardour, or run:"
echo "  ${BIN_DIR}/sd3tool --help"
