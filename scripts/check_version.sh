#!/usr/bin/env bash
#
# Marcel's version is written down in four places that no build step keeps in
# agreement. A release built from a tree where they disagree is a release whose
# artifacts describe themselves differently depending on which one you read, so
# check them together rather than remembering to update each one by hand.
# The Zed revision GPUI is built from is checked the same way; Cargo.lock is
# the only place it can be pinned, so the lock is held to the value Cargo.toml
# records.
#
# Run with a tag name to also check that the tag matches, and that the newest
# AppStream release is not dated before the commit being tagged:
#
#   scripts/check_version.sh v0.1.0

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

fail=0

report() {
	printf '%-44s %s\n' "$1" "$2"
}

canonical="$(sed -nE '0,/^version = "(.+)"$/s//\1/p' Cargo.toml)"
if [ -z "$canonical" ]; then
	echo "Cargo.toml has no package version." >&2
	exit 1
fi

echo "Cargo.toml is the source of truth: $canonical"
echo

check() {
	local label="$1" file="$2" pattern="$3"
	local found
	found="$(sed -nE "$pattern" "$file" | head -n 1)"
	if [ -z "$found" ]; then
		report "$label" "NOT FOUND"
		fail=1
	elif [ "$found" != "$canonical" ]; then
		report "$label" "$found  <- differs"
		fail=1
	else
		report "$label" "$found"
	fi
}

# The lock file records Marcel's own package entry; a stale one means the lock
# was not refreshed after the version bump.
check "Cargo.lock" Cargo.lock \
	'/^name = "marcel"$/{n;s/^version = "(.+)"$/\1/p;}'

check "nix/package.nix" nix/package.nix \
	's/^  version = "(.+)";$/\1/p'

check "AppStream newest release" nix/io.github.berker_z.Marcel.metainfo.xml \
	's/.*<release version="([^"]+)".*/\1/p'

if [ -f CHANGELOG.md ]; then
	check "CHANGELOG.md newest entry" CHANGELOG.md \
		's/^## \[?([0-9]+\.[0-9]+\.[0-9]+)\]?.*/\1/p'
fi


# GPUI comes from the Zed repository at whatever revision the lock records,
# because the dependency lines cannot carry one (see Cargo.toml). A `cargo
# update` that follows Zed's master changes the UI framework underneath the
# release without touching a single line of Marcel, so the lock has to agree
# with the revision written down in Cargo.toml, and with itself.
echo
zed_rev="$(sed -nE 's/^zed-rev = "([0-9a-f]{40})"$/\1/p' Cargo.toml | head -n 1)"
if [ -z "$zed_rev" ]; then
	report "Cargo.toml zed-rev" "NOT FOUND"
	fail=1
else
	locked="$(sed -nE 's|^source = "git\+https://github.com/zed-industries/zed[^#]*#([0-9a-f]{40})"$|\1|p' Cargo.lock | sort -u)"
	case "$(printf '%s\n' "$locked" | grep -c .)" in
	0)
		report "Cargo.lock Zed revision" "NOT FOUND"
		fail=1
		;;
	1)
		if [ "$locked" = "$zed_rev" ]; then
			report "Cargo.lock Zed revision" "$locked"
		else
			report "Cargo.lock Zed revision" "$locked  <- differs from Cargo.toml's $zed_rev"
			fail=1
		fi
		;;
	*)
		report "Cargo.lock Zed revision" "$(printf '%s' "$locked" | tr '\n' ' ') <- more than one"
		fail=1
		;;
	esac
fi
tag="${1-}"
if [ -n "$tag" ]; then
	if [ "$tag" != "v$canonical" ]; then
		report "requested tag" "$tag  <- expected v$canonical"
		fail=1
	else
		report "requested tag" "$tag"
	fi
fi

# Software centres show the release date from the AppStream file, and nothing
# else reads it, so it drifted a month behind the tree once already. The tag
# is immutable, so the date has to be at least the date of the commit being
# tagged. Ordinary commits after a release legitimately postdate it, which is
# why this only fails when a tag was asked for; without one it is a reminder.
echo
release_date="$(sed -nE 's/.*<release version="[^"]+" date="([0-9]{4}-[0-9]{2}-[0-9]{2})".*/\1/p' \
	nix/io.github.berker_z.Marcel.metainfo.xml | head -n 1)"
head_date="$(git log -1 --format=%cs HEAD 2>/dev/null || true)"
if [ -z "$release_date" ]; then
	report "AppStream newest release date" "NOT FOUND"
	fail=1
elif [ -z "$head_date" ]; then
	report "AppStream newest release date" "$release_date  (no git history to compare against)"
elif [ "$release_date" \< "$head_date" ]; then
	if [ -n "$tag" ]; then
		report "AppStream newest release date" "$release_date  <- older than HEAD's $head_date"
		fail=1
	else
		report "AppStream newest release date" "$release_date  (HEAD is $head_date; set the date in the release commit)"
	fi
else
	report "AppStream newest release date" "$release_date"
fi

echo
if [ "$fail" -ne 0 ]; then
	echo "Versions disagree. Fix the entries marked above before tagging." >&2
	exit 1
fi

echo "All recorded versions agree on $canonical."
