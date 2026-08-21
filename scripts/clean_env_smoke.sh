#!/usr/bin/env bash
#
# Launch an installed Marcel with as little inherited environment as the
# release gate's minimal-environment smoke test implies.
#
# The point is to catch the package promising something the host was quietly
# providing. `PATH` is emptied, so `pdftoppm` and `7zz` have to come from the
# wrapper; `XDG_DATA_DIRS` and `XDG_DATA_HOME` point at empty directories, so
# no system icon theme is reachable and the bundled Nordzy subset has to carry
# every icon; `FONTCONFIG_*` is left pointing at a bare config, so the bundled
# Iosevka subset has to be what renders; `HOME` is a fresh directory, so there
# is no existing config, bookmark file, or thumbnail cache.
#
# Wayland, the D-Bus session address, and the runtime dir are kept, because
# without them there is no session to test against.
#
# Usage: scripts/clean_env_smoke.sh /nix/store/...-marcel-rs-0.1.0 [path]

set -euo pipefail

package="${1?usage: clean_env_smoke.sh <package-out-path> [path-to-open]}"
target="${2-}"

if [ ! -x "$package/bin/marcel-rs" ]; then
	echo "no marcel-rs in $package/bin" >&2
	exit 1
fi

root="$(mktemp -d "${TMPDIR:-/tmp}/marcel-clean.XXXXXX")"
mkdir -p "$root/home" "$root/data" "$root/config" "$root/cache" "$root/empty-share"

# A fontconfig setup that knows about no font directory at all. If Marcel
# renders text, it rendered it with the faces it carries.
cat >"$root/fonts.conf" <<'XML'
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <cachedir prefix="xdg">fontconfig</cachedir>
</fontconfig>
XML

echo "clean root: $root"
echo "package:    $package"

exec env -i \
	HOME="$root/home" \
	XDG_DATA_HOME="$root/data" \
	XDG_CONFIG_HOME="$root/config" \
	XDG_CACHE_HOME="$root/cache" \
	XDG_DATA_DIRS="$root/empty-share" \
	XDG_CONFIG_DIRS="$root/empty-share" \
	FONTCONFIG_FILE="$root/fonts.conf" \
	PATH="" \
	WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}" \
	XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}" \
	DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-}" \
	XDG_SESSION_TYPE=wayland \
	"$package/bin/marcel-rs" ${target:+"$target"}
