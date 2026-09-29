#!/usr/bin/env bash
#
# Build, test, and install the PKGBUILD in a fresh Arch container, from the
# tree mounted at /src rather than a release tarball, then check what it
# installed. CI runs it as
#
#   docker run --rm -v "$PWD:/src" archlinux:base-devel /src/packaging/arch/ci.sh
#
# and it runs the same way on any machine with Docker or Podman.

set -euo pipefail

pacman -Syu --noconfirm --needed base-devel appstream desktop-file-utils namcap

# makepkg refuses to run as root, and has to install the dependencies itself.
useradd --create-home builder
echo 'builder ALL=(ALL) NOPASSWD: ALL' >/etc/sudoers.d/builder

work=/home/builder/marcel-rs
mkdir -p "$work"
cp /src/packaging/arch/PKGBUILD "$work/"
pkgver="$(sed -n 's/^pkgver=//p' "$work/PKGBUILD")"

# Stand in for the tag's tarball: the same `marcel-<version>/` prefix GitHub
# gives it, under the name `source=` downloads to, so makepkg uses it as is.
tar -C /src --exclude=./target --exclude=./result --exclude=./.git \
  --transform "s,^\.,marcel-$pkgver," \
  -czf "$work/marcel-rs-$pkgver.tar.gz" .
chown -R builder: "$work"

cd "$work"
sudo -u builder makepkg --syncdeps --noconfirm
# The AUR wants this next to the PKGBUILD, and only makepkg can write it.
sudo -u builder makepkg --printsrcinfo | tee /src/packaging/arch/.SRCINFO
package="$(sudo -u builder makepkg --packagelist | grep -v -- '-debug-' | head -n 1)"
pacman -U --noconfirm "$package"

namcap "$package" || true

test -x /usr/bin/marcel-rs
desktop-file-validate /usr/share/applications/io.github.berker_z.Marcel.desktop \
  /usr/share/applications/marcel.desktop
appstreamcli validate --no-net --explain \
  /usr/share/metainfo/io.github.berker_z.Marcel.metainfo.xml
grep -qx 'Exec=/usr/bin/marcel-rs' \
  /usr/share/dbus-1/services/io.github.berker_z.Marcel.service
grep -qx 'Exec=/usr/bin/marcel-rs' \
  /usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.marcel.service
test -f /usr/share/xdg-desktop-portal/portals/marcel.portal
test -f /usr/share/icons/hicolor/scalable/apps/io.github.berker_z.Marcel.svg
test -d /usr/share/marcel/icons/nordzy
test -f /usr/share/licenses/marcel-rs/LICENSE

# Installing Marcel must not take "show in folder" away from whichever file
# manager has it. The activation file is shipped for copying, not installed.
if grep -rl 'org.freedesktop.FileManager1' /usr/share/dbus-1/services/ \
  | xargs -r pacman -Qo | grep -q marcel-rs; then
  echo "marcel-rs installs a FileManager1 activation file" >&2
  exit 1
fi
test -f /usr/share/marcel/org.freedesktop.FileManager1.service

echo "Installed tree:"
pacman -Ql marcel-rs | grep -v '/icons/nordzy/'
