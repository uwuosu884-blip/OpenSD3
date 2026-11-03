# Maintainer: open-sd3 contributors
pkgname=open-sd3
pkgver=0.3.0
pkgrel=1
pkgdesc="Native Linux VST3/CLAP/Standalone multi-mic drum workstation & Superior Drummer 3 (SL-* / .obw / ttfs) inspector/extractor"
arch=('x86_64')
license=('GPL-3.0-or-later')
depends=('alsa-lib' 'libx11' 'libxcb' 'xcb-util-wm' 'libxkbcommon-x11' 'libxcursor' 'libglvnd' 'gcc-libs')
makedepends=('rust' 'cargo' 'git' 'pkgconf')
optdepends=(
  'jack: JACK/PipeWire-JACK audio backend for standalone open-sd3'
  'sfizz: Alternative SFZ playback for kits exported via sd3tool extract'
)
source=()
sha256sums=()

build() {
  cd "${startdir}"
  cargo run --package xtask -- bundle open-sd3 --release
  cargo build --release -p sd3-cli -p open-sd3 --bins
}

check() {
  cd "${startdir}"
  cargo test --workspace
}

package() {
  cd "${startdir}"
  install -Dm755 "target/release/sd3tool" "${pkgdir}/usr/bin/sd3tool"
  install -Dm755 "target/release/open-sd3" "${pkgdir}/usr/bin/open-sd3"
  install -Dm755 "target/bundled/open-sd3.clap" "${pkgdir}/usr/lib/clap/open-sd3.clap"
  install -dm755 "${pkgdir}/usr/lib/vst3"
  cp -r "target/bundled/open-sd3.vst3" "${pkgdir}/usr/lib/vst3/"
}
