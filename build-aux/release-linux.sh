#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Builds release tarballs of cera and cera-cli for Linux x86_64, i686 and aarch64 in Debian 13
# (trixie) containers, so the binaries need glibc 2.41, GTK 4.18 and libadwaita 1.7 at most.
# i686 and aarch64 are cross-compiled against Debian's multiarch libraries. Uses the host's
# stable Rust toolchain and cargo cache, mounted into the container.
#
#   build-aux/release-linux.sh                    all three
#   build-aux/release-linux.sh aarch64            just one
#
# Output: target/dist/cera-<version>-linux-<arch>.tar.xz. Needs docker and rustup.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n1)
RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
TOOLCHAIN=$(rustup run stable rustc -vV | sed -n 's/^host: //p')
DIST=$ROOT/target/dist

build() {
    arch=$1
    case $arch in
        x86_64) deb=amd64 triple=x86_64-unknown-linux-gnu gnu=x86_64-linux-gnu ;;
        i686) deb=i386 triple=i686-unknown-linux-gnu gnu=i686-linux-gnu ;;
        aarch64) deb=arm64 triple=aarch64-unknown-linux-gnu gnu=aarch64-linux-gnu ;;
        *) echo "unknown arch: $arch" >&2; exit 1 ;;
    esac
    libdir=$(echo "$gnu" | sed 's/^i686/i386/')
    image=cera-build:trixie-$deb
    rustup target add --toolchain stable "$triple"

    if [ "$deb" = amd64 ]; then cross=; else cross="gcc-$gnu libc6-dev-$deb-cross"; fi
    docker build -t "$image" - <<EOF
FROM debian:trixie
RUN dpkg --add-architecture $deb && apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates gcc libc6-dev pkgconf \
      libglib2.0-dev-bin $cross \
      libgtk-4-dev:$deb libadwaita-1-dev:$deb libpoppler-glib-dev:$deb \
 && rm -rf /var/lib/apt/lists/*
EOF

    upper=$(echo "$triple" | tr 'a-z-' 'A-Z_')
    docker run --rm -u "$(id -u):$(id -g)" \
        -v "$ROOT:/src" -v "$RUSTUP_HOME:/rustup:ro" -v "$CARGO_HOME:/cargo" -w /src \
        -e HOME=/tmp -e CARGO_HOME=/cargo -e RUSTUP_HOME=/rustup \
        -e PATH="/rustup/toolchains/stable-$TOOLCHAIN/bin:/usr/bin:/bin" \
        -e CARGO_TARGET_DIR="/src/target/dist/build-$arch" \
        -e CARGO_PROFILE_RELEASE_STRIP=symbols \
        -e "CARGO_TARGET_${upper}_LINKER=$gnu-gcc" \
        -e PKG_CONFIG_ALLOW_CROSS=1 \
        -e "PKG_CONFIG_LIBDIR=/usr/lib/$libdir/pkgconfig:/usr/share/pkgconfig" \
        "$image" cargo build --release --locked --target "$triple" -p cera -p cera-cli

    bin=$DIST/build-$arch/$triple/release
    name=cera-$VERSION-linux-$arch
    stage=$DIST/$name
    rm -rf "$stage"
    DESTDIR=$stage PREFIX=/ CERA_BIN=$bin/cera "$ROOT/build-aux/install.sh"
    install -Dm755 "$bin/cera-cli" "$stage/bin/cera-cli"
    install -Dm644 "$ROOT/README.md" "$stage/share/doc/cera/README.md"
    install -Dm644 "$ROOT/COPYING" "$stage/share/licenses/cera/COPYING"
    tar -C "$DIST" -cJf "$DIST/$name.tar.xz" "$name"
    echo "$DIST/$name.tar.xz"
}

mkdir -p "$DIST"
for arch in ${*:-x86_64 i686 aarch64}; do
    build "$arch"
done
