#!/usr/bin/env bash
# Packs the release build into what a release offers: an archive, a Debian
# package and an AppImage, in dist/. Run after `cargo build --release`; the
# release workflow does exactly this, then signs the files.
#
# Usage: packaging/build-packages.sh VERSION
set -euo pipefail
umask 022
cd "$(dirname "$0")/.."

version=${1:?usage: packaging/build-packages.sh VERSION}
arch=$(uname -m)
case $arch in
    x86_64) deb_arch=amd64 ;;
    aarch64) deb_arch=arm64 ;;
    *) echo "unsupported architecture $arch" >&2; exit 1 ;;
esac
bin=target/release
for program in anything anythingd any; do
    [ -x "$bin/$program" ] || { echo "build first: cargo build --release" >&2; exit 1; }
done

rm -rf dist
mkdir -p dist
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Fills the path placeholders of a packaging template.
fill() { sed -e "s|@SERVICE@|$1|g" -e "s|@APP@|$2|g" "$3"; }

# --- Archive: the three programs, for "Install for all users" and updates.
name=anything-$version-$arch-linux
mkdir "$work/$name"
cp "$bin/anything" "$bin/anythingd" "$bin/any" README.md LICENSE-MIT LICENSE-APACHE "$work/$name/"
strip "$work/$name/anything" "$work/$name/anythingd" "$work/$name/any"
tar -C "$work" -czf "dist/$name.tar.gz" "$name"

# --- Debian package. The service starts right away.
deb=$work/deb
service=/usr/libexec/anything/anythingd
mkdir -p "$deb/DEBIAN" "$deb/usr/bin" "$deb/usr/libexec/anything" "$deb/usr/lib/systemd/system" \
    "$deb/usr/share/polkit-1/actions" "$deb/usr/share/applications" \
    "$deb/usr/share/icons/hicolor/scalable/apps" "$deb/usr/share/doc/anything" "$deb/etc/anything"
install -m 755 "$work/$name/anything" "$work/$name/any" "$deb/usr/bin/"
install -m 755 "$work/$name/anythingd" "$deb$service"
fill "$service" /usr/bin/anything packaging/anythingd.service > "$deb/usr/lib/systemd/system/anythingd.service"
fill "$service" /usr/bin/anything packaging/io.github.srcmkr.anything.policy \
    > "$deb/usr/share/polkit-1/actions/io.github.srcmkr.anything.policy"
fill "$service" /usr/bin/anything packaging/anything.desktop > "$deb/usr/share/applications/anything.desktop"
install -m 644 crates/anything-gui/assets/anything.svg "$deb/usr/share/icons/hicolor/scalable/apps/anything.svg"
install -m 644 packaging/anythingd.toml "$deb/etc/anything/anythingd.toml"
cat LICENSE-MIT LICENSE-APACHE > "$deb/usr/share/doc/anything/copyright"
chmod 644 "$deb"/usr/lib/systemd/system/* "$deb"/usr/share/polkit-1/actions/* \
    "$deb"/usr/share/applications/* "$deb/usr/share/doc/anything/copyright"

cat > "$deb/DEBIAN/control" <<EOF
Package: anything
Version: $version
Section: utils
Priority: optional
Architecture: $deb_arch
Maintainer: srcmkr <srcmkr@users.noreply.github.com>
Homepage: https://github.com/srcmkr/anything
Depends: libc6 (>= 2.35), libgcc-s1, libxkbcommon0, libxkbcommon-x11-0, libxcb1, libfontconfig1, libwayland-client0, libvulkan1
Recommends: pkexec | policykit-1, bubblewrap
Installed-Size: $(du -sk "$deb" | cut -f1)
Description: Instant file name search, in the spirit of Everything
 Click the tray icon, type, and millions of files narrow down while you type.
 A system service indexes every local disk and keeps the index current live;
 each user only sees what they could open themselves.
EOF
echo /etc/anything/anythingd.toml > "$deb/DEBIAN/conffiles"

cat > "$deb/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    if [ -e /etc/systemd/system/anythingd.service ]; then
        echo "anything: a search service installed from Anything's settings is in the way;"
        echo "          remove it with: sudo /usr/local/libexec/anything/anythingd --uninstall"
    fi
    systemctl enable anythingd.service >/dev/null 2>&1 || true
    systemctl restart anythingd.service >/dev/null 2>&1 || true
fi
EOF
cat > "$deb/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ]; then
    systemctl disable --now anythingd.service >/dev/null 2>&1 || true
fi
EOF
cat > "$deb/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
systemctl daemon-reload >/dev/null 2>&1 || true
# The index holds every user's file names: it goes with the package.
if [ "$1" = purge ] || [ "$1" = remove ]; then
    rm -rf /var/lib/anything /var/cache/anything
fi
EOF
chmod 755 "$deb/DEBIAN/postinst" "$deb/DEBIAN/prerm" "$deb/DEBIAN/postrm"
dpkg-deb --root-owner-group -Zxz --build "$deb" "dist/anything_${version}_${deb_arch}.deb" >/dev/null

# --- AppImage: one file to download and run.
app=$work/Anything.AppDir
mkdir -p "$app/usr/bin"
install -m 755 "$work/$name/anything" "$work/$name/anythingd" "$work/$name/any" "$app/usr/bin/"
cat > "$app/AppRun" <<'EOF'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/anything" "$@"
EOF
chmod 755 "$app/AppRun"
sed -e 's|^Exec=@APP@ --show|Exec=anything --show|' packaging/anything.desktop > "$app/anything.desktop"
cp crates/anything-gui/assets/anything.svg "$app/anything.svg"
ln -s anything.svg "$app/.DirIcon"

tool=${APPIMAGETOOL:-$work/appimagetool}
if [ ! -x "$tool" ]; then
    curl -sSfL -o "$tool" "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$arch.AppImage"
    chmod 755 "$tool"
fi
ARCH=$arch "$tool" --appimage-extract-and-run --no-appstream "$app" "dist/Anything-$version-$arch.AppImage" >/dev/null 2>&1

# The same under names without the version, so links to the latest release
# (README) always work.
cp "dist/anything_${version}_${deb_arch}.deb" "dist/anything_${deb_arch}.deb"
cp "dist/Anything-$version-$arch.AppImage" "dist/Anything-$arch.AppImage"

( cd dist && sha256sum -- * > SHA256SUMS )
ls -l dist
