#!/usr/bin/env bash
# Package an already-built x86_64 GNU/Linux binary; no build or installation.
set -euo pipefail
umask 022
export LC_ALL=C TZ=UTC
if [[ $# != 3 ]]; then
    echo "Usage: $0 BINARY VERSION OUTPUT_DIRECTORY" >&2
    exit 2
fi
binary=$(realpath -- "$1")
version=$2
mkdir -p -- "$3"
out=$(realpath -- "$3")
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([~+-][0-9A-Za-z.+~-]+)?$ ]] || { echo 'Invalid package version' >&2; exit 2; }
readelf -h "$binary" | grep -q 'Machine:.*Advanced Micro Devices X86-64'
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct)}
[[ "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]]
stage=$(mktemp -d "$out/.pneu-package.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
pkg=$stage/deb
bundle=$stage/pneu-linux-x86_64
install -Dm755 "$binary" "$pkg/usr/bin/pneud"
strip --strip-unneeded "$pkg/usr/bin/pneud"
install -Dm644 "$root/packaging/pneu.desktop" "$pkg/usr/share/applications/pneu.desktop"
install -Dm644 "$root/assets/pneu.svg" "$pkg/usr/share/icons/hicolor/scalable/apps/pneu.svg"
install -Dm644 "$root/packaging/pneu-send.desktop" "$pkg/usr/share/kio/servicemenus/pneu-send.desktop"
mkdir -p "$pkg/usr/lib/systemd/user" "$pkg/DEBIAN" "$stage/debian"
sed 's|^ExecStart=%h/.local/bin/pneud run$|ExecStart=/usr/bin/pneud run|' \
    "$root/packaging/pneud.service" > "$pkg/usr/lib/systemd/user/pneud.service"
grep -qx 'ExecStart=/usr/bin/pneud run' "$pkg/usr/lib/systemd/user/pneud.service"
# shlibdeps needs source-package metadata, but never touches the checkout.
cat > "$stage/debian/control" <<'CONTROL'
Source: pneu
Section: utils
Priority: optional
Maintainer: Matthew Stanton <stantonmatthewj@gmail.com>

Package: pneu
Architecture: amd64
Description: Direct file and clipboard sharing between your devices
CONTROL
shlibs=$(cd "$stage" && dpkg-shlibdeps -O -e"$pkg/usr/bin/pneud")
shlibs=${shlibs#shlibs:Depends=}
[[ -n "$shlibs" && "$shlibs" != *'$'* ]]
# Ubuntu 24.04 renamed Qt Gui/Widgets during the time64 transition;
# Debian 13 and Ubuntu 26.04 use the original names for these same amd64
# SONAMEs. Keep the shlibdeps-derived minimum on both alternatives. Core
# still uses its t64 package name on all three distributions.
shlibs=$(printf '%s\n' "$shlibs" | sed -E 's/(libqt6(gui|widgets)6)t64( \([^)]*\))?/\1t64\3 | \1\3/g')
cat > "$pkg/DEBIAN/control" <<CONTROL
Package: pneu
Version: $version
Architecture: amd64
Maintainer: Matthew Stanton <stantonmatthewj@gmail.com>
Section: utils
Priority: optional
Homepage: https://stantonmatt.github.io/pneu/
Installed-Size: 0
Depends: $shlibs, init-system-helpers (>= 1.52), qt6-qpa-plugins, qt6-wayland
Recommends: systemd, dbus-user-session, xdg-desktop-portal
Description: Direct file and clipboard sharing between your devices
 Pneu pairs Android phones and computers for encrypted file transfers and
 clipboard sharing. The Linux desktop integration supports Plasma 6 on Wayland.
CONTROL
for script in postinst postrm; do
    install -m755 "$root/packaging/linux/$script" "$pkg/DEBIAN/$script"
done
install -Dm644 "$root/packaging/linux/pneud.1" "$pkg/usr/share/man/man1/pneud.1"
gzip -n -9 "$pkg/usr/share/man/man1/pneud.1"
install -Dm644 "$root/../LICENSE" "$pkg/usr/share/doc/pneu/copyright"
cat > "$stage/changelog" <<CHANGELOG
pneu ($version) stable; urgency=medium

  * Upstream release.

 -- Matthew Stanton <stantonmatthewj@gmail.com>  $(date -R -d "@$SOURCE_DATE_EPOCH")
CHANGELOG
gzip -n -9 -c "$stage/changelog" > "$pkg/usr/share/doc/pneu/changelog.gz"
sed -i "s/^Installed-Size: .*/Installed-Size: $(du -sk "$pkg/usr" | cut -f1)/" "$pkg/DEBIAN/control"
(cd "$pkg" && find usr -type f -print0 | sort -z | xargs -0 md5sum) > "$pkg/DEBIAN/md5sums"
# Both installers consume the same assets and original per-user unit.
install -Dm755 "$pkg/usr/bin/pneud" "$bundle/pneud"
install -Dm755 "$root/packaging/linux/install-binary.sh" "$bundle/install.sh"
install -m644 "$root/packaging/linux/install-user.sh" "$bundle/install-user.sh"
install -Dm644 "$root/assets/pneu.svg" "$bundle/assets/pneu.svg"
for file in pneu.desktop pneu-send.desktop pneud.service; do
    install -Dm644 "$root/packaging/$file" "$bundle/packaging/$file"
done
install -m644 "$root/../LICENSE" "$bundle/LICENSE"
install -m644 "$root/../docs/linux.md" "$bundle/README.md"
find "$pkg" "$bundle" -print0 | xargs -0 touch --no-dereference -d "@$SOURCE_DATE_EPOCH"
dpkg-deb --root-owner-group --threads-max=1 -Zxz -z9 --build "$pkg" "$out/pneu_${version}_amd64.deb"
tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
    --format=gnu -C "$stage" -cf - pneu-linux-x86_64 | gzip -n -9 > "$out/pneu-linux-x86_64.tar.gz"
printf '%s\n' "$out/pneu_${version}_amd64.deb" "$out/pneu-linux-x86_64.tar.gz"
