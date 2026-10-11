#!/usr/bin/env bash
set -euo pipefail
project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
cargo build --release --locked
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)
architecture=$(dpkg --print-architecture)
bundle="yolocam-linux-$version-linux-$(uname -m)"
# Recreate generated staging directories so removed files cannot leak into a
# later release. The finished archives stay in dist until they are replaced.
rm -rf -- "dist/$bundle" dist/deb
mkdir -p "dist/$bundle" "dist/deb/usr/bin" "dist/deb/usr/share/applications" \
    "dist/deb/usr/share/icons/hicolor/scalable/apps" "dist/deb/usr/share/doc/yolocam-linux" "dist/deb/DEBIAN"
install -m755 target/release/yolocam-linux "dist/$bundle/yolocam-linux"
cp LICENSE NOTICE THIRD_PARTY_NOTICES.md README.md "dist/$bundle/"
mkdir -p "dist/$bundle/packaging"
install -m755 packaging/install-local.sh "dist/$bundle/packaging/"
cp packaging/yolocam-linux.desktop packaging/yolocam-linux.svg "dist/$bundle/packaging/"
cp -R packaging/licenses "dist/$bundle/packaging/"
cp -R docs "dist/$bundle/"
install -m755 target/release/yolocam-linux dist/deb/usr/bin/yolocam-linux
install -m644 packaging/yolocam-linux.desktop dist/deb/usr/share/applications/
install -m644 packaging/yolocam-linux.svg dist/deb/usr/share/icons/hicolor/scalable/apps/
cp LICENSE NOTICE THIRD_PARTY_NOTICES.md dist/deb/usr/share/doc/yolocam-linux/
cp -R packaging/licenses dist/deb/usr/share/doc/yolocam-linux/
cat > dist/deb/DEBIAN/control <<EOF
Package: yolocam-linux
Version: $version
Architecture: $architecture
Maintainer: YoloCam Linux contributors <noreply@github.com>
Section: video
Priority: optional
Depends: libc6 (>= 2.39), libgcc-s1, libx11-6, libxkbcommon0, libwayland-client0, libegl1, libgl1
Homepage: https://github.com/rwfresh/yolocam-linux
Description: Native YoloCam S3 controls, preview and firmware updates
 An independent community Linux camera app written in Rust.
 Uses Linux UVC and USB network drivers and supports native vendor controls.
EOF
tar --exclude=__pycache__ --exclude='*.pyc' -C dist -czf "dist/$bundle.tar.gz" "$bundle"
dpkg-deb --root-owner-group --build dist/deb "dist/yolocam-linux_${version}_${architecture}.deb"
(cd dist && sha256sum "$bundle.tar.gz" "yolocam-linux_${version}_${architecture}.deb" > SHA256SUMS)
