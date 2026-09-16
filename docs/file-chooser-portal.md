# Being the file picker

Marcel handles "show in folder" and `xdg-open` on a directory. It does not
handle the open-file dialog or the save dialog, and those two are the same
thing: both come from the `FileChooser` interface of xdg-desktop-portal.
Right now the GTK portal backend answers them, which is why a GNOME-looking
dialog pops up in the middle of a Hyprland session. This note is what it
would take for Marcel to answer them instead.

## How the picker is chosen

Applications do not open a file dialog themselves any more. They call
`org.freedesktop.portal.FileChooser` on the session bus. The portal frontend
(`xdg-desktop-portal`) reads `portals.conf`, finds which backend is
configured for `org.freedesktop.impl.portal.FileChooser`, and forwards the
call to it. The backend puts up a window, the user picks something, and the
URIs travel back the same way.

A backend is just another D-Bus service that owns
`org.freedesktop.impl.portal.desktop.<name>` and ships a
`<name>.portal` file under `share/xdg-desktop-portal/portals/` listing the
interfaces it implements. That is the whole registration. On this machine
the configured backends are `hyprland` (screencast, global shortcuts) and
`gtk` (everything else, including FileChooser), and nothing names a
FileChooser backend explicitly, so the frontend falls through to GTK.

Marcel already has the pieces for the other side of this. `system_open.rs`
calls the portal as a client, and `desktop_integration.rs` owns a bus name
and implements `org.freedesktop.FileManager1` over zbus. A FileChooser
backend is the same shape of code pointed the other way.

## What Marcel has to implement

The interface is `org.freedesktop.impl.portal.FileChooser`, version 4, on
the object path `/org/freedesktop/portal/desktop`, under the bus name
`org.freedesktop.impl.portal.desktop.marcel`. Three methods:

- `OpenFile(handle, app_id, parent_window, title, options) -> (response, results)`
- `SaveFile(handle, app_id, parent_window, title, options) -> (response, results)`
- `SaveFiles(handle, app_id, parent_window, title, options) -> (response, results)`

`response` is 0 for success, 1 for the user cancelling, 2 for anything else.
`results` is a dict; the entry that matters is `uris` (an array of `file://`
strings). `choices` and `current_filter` go in there too if the request
asked for them.

The `options` dict is where the requests differ. `OpenFile` can ask for
`multiple`, `directory` (pick a folder rather than a file; this is why
version 4 matters), `filters` (name plus a list of glob or MIME patterns),
`current_filter`, and `choices` (extra combo boxes or checkboxes the app
wants in the dialog). `SaveFile` adds `current_name`, `current_folder` and
`current_file`, which are what the app suggests the dialog start with.
`SaveFiles` is the odd one: the app hands over a list of files it wants to
write and asks for a single destination folder. Every option is optional,
and a backend that ignores `choices` entirely still works; it just returns
no `choices` in the results.

`handle` is an object path the frontend creates for the request. The backend
is expected to export an `org.freedesktop.impl.portal.Request` object at
that path with a single `Close` method, so the frontend can cancel the
dialog if the calling app goes away. `parent_window` is a string like
`wayland:<xdg_foreign handle>` meant for making the dialog modal to the
caller. GPUI does not expose xdg_foreign, so the first version should
simply ignore it and open a normal top-level window. That is what
termfilechooser does and nothing complains.

For Marcel that means a picker window: a Marcel pane that opens at
`current_folder` (or the home directory), applies the filters if any,
allows single or multiple selection according to `multiple`, and has a
confirm button. Save mode needs a filename field seeded with
`current_name`, and a confirmation when the target exists. Directory mode
confirms the current folder rather than a selection inside it. The method
call blocks until the window closes, so the D-Bus handler awaits a oneshot
from the window and returns whatever it sends.

The `.portal` file is small:

```ini
[portal]
DBusName=org.freedesktop.impl.portal.desktop.marcel
Interfaces=org.freedesktop.impl.portal.FileChooser;
UseIn=Hyprland;
```

`UseIn` is only consulted when nothing in `portals.conf` names the backend,
and we will name it, so it is there for completeness.

The Nix side is a new output next to `file-manager1-service`: the same
wrapper with an environment variable that tells Marcel to claim the portal
bus name, plus the `.portal` file and a D-Bus activation file so the
frontend can start Marcel on demand. In dotfiles it becomes

```nix
xdg.portal.extraPortals = [pkgs.marcel-portal];
xdg.portal.config.common."org.freedesktop.impl.portal.FileChooser" = ["marcel"];
```

and `xdg-desktop-portal` starts routing every picker to Marcel.

## Things that will bite

The frontend talks to exactly one backend per interface and expects it to
answer. If Marcel crashes mid-dialog the app that asked sees an error and
usually shows nothing. Worth a fallback in `portals.conf`
(`["marcel" "gtk"]`) while it is young; the frontend uses the next entry
when the first is not on the bus.

Browsers are not uniform about using the portal. Firefox and Zen use it
when `widget.use-xdg-desktop-portal.file-picker` is `1`; the default `2`
means "only under GNOME or in a sandbox". Chromium-based browsers have
switched to the portal picker in recent releases, but if Helium keeps
showing a GTK dialog after the switch, that is the first thing to check
rather than the backend.

Cold start matters here more than for FileManager1. A save dialog that
takes two seconds to appear because Marcel had to launch is noticeable. The
one-process-per-session design already covers this once Marcel is open;
the D-Bus activation file covers the case where it is not.

## Checking it works without a browser

The frontend can be poked directly:

```sh
busctl --user call org.freedesktop.portal.Desktop \
  /org/freedesktop/portal/desktop \
  org.freedesktop.portal.FileChooser OpenFile 'ssa{sv}' '' 'Pick something' 0
```

That goes through the real routing, so a dialog from the configured backend
should appear and the chosen URI comes back on a `Response` signal.
`busctl --user status org.freedesktop.impl.portal.desktop.marcel` says
whether Marcel is currently the one holding the name.

## Prior art

[xdg-desktop-portal-termfilechooser](https://github.com/GermainZ/xdg-desktop-portal-termfilechooser)
is a backend that opens a terminal running yazi or ranger as the picker.
It is small, C, and covers exactly the three methods above with the
`Request` object and cancellation; a good reference for the D-Bus surface.
The interface itself is documented in the
[portal backend reference](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.impl.portal.FileChooser.html).
