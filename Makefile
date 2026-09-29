# Where Marcel's files go once it is built. Every package uses this: the flake
# calls `install-data` from `nix/package.nix`, and the Arch PKGBUILD calls
# `install`. If a file is missing from one package, it is missing from here.
#
#   make install PREFIX=/usr DESTDIR="$pkgdir"
#
# BINARY     the built executable (default: target/release/marcel-rs)
# SEVENZIP   a 7-Zip to link as libexec/marcel/7zz, where Marcel looks first.
#            Leave empty to use whichever `7zz` or `7z` is on PATH.
# PORTAL=0   leave out the file-chooser portal files. The flake does, because
#            its portal variant installs them pointing at its own wrapper.
#
# Nothing here claims `org.freedesktop.FileManager1`. Its activation file goes
# to share/marcel for a user to copy into ~/.local/share/dbus-1/services;
# installed system-wide it would collide with Nautilus's and take over "show
# in folder" without anyone asking.
#
# The licenses go along because MIT wants its notice to travel with every
# copy, and the Yazi notice in THIRD_PARTY_NOTICES.md is there for the same
# reason; Nordzy and Iosevka carry their own terms.

PREFIX ?= /usr/local
DESTDIR ?=
BINARY ?= target/release/marcel-rs
SEVENZIP ?=
PORTAL ?= 1

bindir = $(DESTDIR)$(PREFIX)/bin
datadir = $(DESTDIR)$(PREFIX)/share
libexecdir = $(DESTDIR)$(PREFIX)/libexec

APP_ID = io.github.berker_z.Marcel

.PHONY: all install install-bin install-data uninstall

all:
	cargo build --release --locked

install: install-bin install-data

install-bin:
	install -Dm755 "$(BINARY)" "$(bindir)/marcel-rs"

install-data:
	install -Dm644 -t "$(datadir)/applications" \
		packaging/$(APP_ID).desktop packaging/marcel.desktop
	install -Dm644 -t "$(datadir)/metainfo" packaging/$(APP_ID).metainfo.xml
	install -Dm644 -t "$(datadir)/dbus-1/interfaces" packaging/org.freedesktop.FileManager1.xml
	install -d "$(datadir)/dbus-1/services" "$(datadir)/marcel"
	sed 's|@marcel@|$(PREFIX)|' packaging/$(APP_ID).service \
		> "$(datadir)/dbus-1/services/$(APP_ID).service"
	sed 's|@marcel@|$(PREFIX)|' packaging/org.freedesktop.FileManager1.service \
		> "$(datadir)/marcel/org.freedesktop.FileManager1.service"
	chmod 644 "$(datadir)/dbus-1/services/$(APP_ID).service" \
		"$(datadir)/marcel/org.freedesktop.FileManager1.service"
ifeq ($(PORTAL),1)
	install -Dm644 -t "$(datadir)/xdg-desktop-portal/portals" packaging/marcel.portal
	sed 's|@marcel@|$(PREFIX)|' packaging/org.freedesktop.impl.portal.desktop.marcel.service \
		> "$(datadir)/dbus-1/services/org.freedesktop.impl.portal.desktop.marcel.service"
	chmod 644 "$(datadir)/dbus-1/services/org.freedesktop.impl.portal.desktop.marcel.service"
endif
	install -d "$(datadir)/icons" "$(datadir)/marcel/icons"
	cp -R --no-preserve=mode,ownership assets/icons/hicolor "$(datadir)/icons/"
	cp -R --no-preserve=mode,ownership assets/icons/nordzy "$(datadir)/marcel/icons/"
	install -Dm644 -t "$(datadir)/licenses/marcel" LICENSE THIRD_PARTY_NOTICES.md \
		assets/fonts/OFL-Iosevka.md
	install -Dm644 assets/icons/nordzy/COPYING "$(datadir)/licenses/marcel/COPYING-Nordzy"
ifneq ($(SEVENZIP),)
	install -d "$(libexecdir)/marcel"
	ln -sf "$(SEVENZIP)" "$(libexecdir)/marcel/7zz"
endif

uninstall:
	rm -f "$(bindir)/marcel-rs" \
		"$(datadir)/applications/$(APP_ID).desktop" \
		"$(datadir)/applications/marcel.desktop" \
		"$(datadir)/metainfo/$(APP_ID).metainfo.xml" \
		"$(datadir)/dbus-1/interfaces/org.freedesktop.FileManager1.xml" \
		"$(datadir)/dbus-1/services/$(APP_ID).service" \
		"$(datadir)/dbus-1/services/org.freedesktop.impl.portal.desktop.marcel.service" \
		"$(datadir)/xdg-desktop-portal/portals/marcel.portal" \
		"$(libexecdir)/marcel/7zz"
	find "$(datadir)/icons/hicolor" -name '$(APP_ID).*' -delete
	rm -rf "$(datadir)/marcel" "$(datadir)/licenses/marcel"
	rmdir "$(libexecdir)/marcel" 2>/dev/null || true
