# Maintainer: David Kaantun <dkantun@gmail.com>
pkgname=cosmic-ext-show-desktop
pkgver=0.1.0
pkgrel=1
pkgdesc='COSMIC applet that minimizes/restores all windows (Show Desktop)'
arch=('x86_64')
url='https://github.com/Gargadonc/cosmic-ext-show-desktop'
license=('GPL-3.0-or-later')
depends=(
    'cosmic-randr'
    'libxkbcommon'
    'fontconfig'
    'ttf-font'
)
makedepends=('cargo' 'rust')
source=("$pkgname-$pkgver.tar.gz::${url}/archive/v${pkgver}.tar.gz")
sha256sums=('SKIP')

prepare() {
    cd "$srcdir/$pkgname-$pkgver"
    cargo fetch --locked
}

build() {
    cd "$srcdir/$pkgname-$pkgver"
    export CARGO_TARGET_DIR="$srcdir/target"
    cargo build --release --frozen
}

package() {
    cd "$srcdir/$pkgname-$pkgver"
    install -Dm755 "$srcdir/target/release/$pkgname" \
        "$pkgdir/usr/bin/$pkgname"
    install -Dm644 data/com.system76.CosmicAppletShowDesktop.desktop \
        "$pkgdir/usr/share/applications/com.system76.CosmicAppletShowDesktop.desktop"
}
