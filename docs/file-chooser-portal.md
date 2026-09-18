# Being the file picker

Marcel handles "show in folder" and `xdg-open` on a directory, and it now
handles the open-file and save-file dialogs too. Those two are the same thing:
both come from the `FileChooser` interface of xdg-desktop-portal. Before this,
the GTK portal backend answered them, which is why a GNOME-looking dialog used
to pop up in the middle of a Hyprland session. This note is how Marcel answers
them instead: the wire protocol, where the code lives, how it is packaged, and
what to check when a dialog does not appear.

## How the picker is chosen

Applications do not open a file dialog themselves any more. They call
`org.freedesktop.portal.FileChooser` on the session bus. The portal frontend
(`xdg-desktop-portal`) reads `portals.conf`, finds which backend is configured
for `org.freedesktop.impl.portal.FileChooser`, and forwards the call to it. The
backend puts up a window, the user picks something, and the URIs travel back
the same way.

A backend is a D-Bus service that owns `org.freedesktop.impl.portal.desktop.<name>`
and ships a `<name>.portal` file under `share/xdg-desktop-portal/portals/`
listing the interfaces it implements. That is the whole registration. The
frontend loads every `.portal` file it can find, then walks the names in
`portals.conf` and takes the first one whose file it loaded. It does not check
whether that name is on the bus; that is D-Bus activation's job later. So
naming `marcel` is enough once the portal file is installed, and a second entry
after it would only ever be consulted if Marcel's portal file were missing,
not if Marcel failed to answer.

`nix/marcel.portal` is three lines: the bus name and the one interface. It
used to carry `UseIn=Hyprland;` as well, the pre-1.17 way of saying which
desktops a backend was for. xdg-desktop-portal 1.17 replaced that with
`portals.conf` and deprecated the key, and a backend that still sets it is
only ever reached through the deprecated path, and then only under the
desktops it names. Dropping it means Marcel is chosen by exactly one thing,
the `portals.conf` entry the module writes, on any desktop. Without any
configuration the frontend falls back to whatever loaded backend implements
the interface and warns about it; if that fallback is what you are relying
on, write the config line instead.

## The protocol

The interface is `org.freedesktop.impl.portal.FileChooser`, version 4, on the
object path `/org/freedesktop/portal/desktop`, under the bus name
`org.freedesktop.impl.portal.desktop.marcel`. Three methods:

- `OpenFile(handle, app_id, parent_window, title, options) -> (response, results)`
- `SaveFile(handle, app_id, parent_window, title, options) -> (response, results)`
- `SaveFiles(handle, app_id, parent_window, title, options) -> (response, results)`

`response` is 0 for success, 1 for the user cancelling, 2 for anything else.
`results` is a dict; the entry that matters is `uris` (an array of `file://`
strings). Marcel also returns `current_filter` when the request offered
filters, in the same `(s a(us))` shape it arrived in.

The `options` dict is where the requests differ. `OpenFile` can ask for
`multiple`, `directory` (pick a folder rather than a file; this is why
version 4 matters), `filters` (a name plus a list of glob or MIME patterns),
`current_filter`, and `accept_label`. `SaveFile` adds `current_name`,
`current_folder`, and `current_file`, which are what the app suggests the
dialog start with. `SaveFiles` hands over a list of file names and asks for a
single folder to write them into; the answer is one URI per name, in order.
Every option is optional. `choices` (extra combo boxes an app wants in the
dialog) is accepted and ignored, and no `choices` come back, which the spec
allows.

`handle` is an object path the frontend chooses for the request. Marcel
exports an `org.freedesktop.impl.portal.Request` object there for exactly as
long as the call is pending. Its one method, `Close`, is how the frontend
withdraws a dialog whose caller went away: the window closes itself and the
call returns 2. `parent_window` is a `wayland:<xdg_foreign handle>` string
meant for making the dialog modal to the caller. GPUI does not expose
xdg_foreign, so Marcel ignores it and opens a normal top-level window. That is
what termfilechooser does and nothing complains.

## Where the code lives

`src/desktop/file_chooser.rs` is the D-Bus side. It decodes the options
dictionary into a `PickerRequest`, hands it to the application over a bounded channel,
and awaits the answer; the method call blocks until a window replies. Every
caller-supplied value is bounded and type-checked, and a wrong type is an
`InvalidArgs` error rather than a silently ignored option. The `files` of a
`SaveFiles` request go through the same name validation as Rename, so a name
carrying a directory cannot write outside the folder the user chose.

`src/desktop/picker.rs` is the request model with no D-Bus in it: modes,
filters, the response. Filters use `globset`, matched case-insensitively on purpose.
Applications write `*.[jJ][pP][gG]` because GTK matches case-sensitively, and a
picker that hides `Photo.JPG` behind `*.jpg` is wrong on a filesystem where the
user never chose the case. MIME patterns go through `mime_guess` on the file
name and accept `image/*` families. Folders always pass a filter, or nothing
could be navigated into.

`window::open_picker` opens the window. It is an ordinary Marcel pane (same
browser, same preview, same operations, same bookmarks) built by
`Marcel::new_picker`, with a bar along the bottom holding the name field of a
save dialog, the filter dropdown, Cancel, and the accept button. Pickers are
kept apart from browsing windows in the registry, so a "show in folder"
request never navigates a dialog somebody is answering, and a raised
`Activate` never counts a dialog as "the Marcel I have". There are at most
eight open at once; past that a request is answered with 2 immediately.

What changes inside the pane:

- A single-file picker restricts the selection model to one item, so no
  gesture (range, marquee, select all, reveal) can show three highlighted
  rows and hand back one.
- Double-clicking or pressing Enter on a file answers the dialog instead of
  launching the file. On a file that is part of the selection, the answer is
  the whole selection.
- In a save dialog, clicking a file proposes its name, Enter in the name field
  confirms, and typing a folder's name goes into it. If the target exists you
  are asked before it is replaced. The existence check runs off the foreground
  thread, like every other stat in Marcel.
- In directory mode, Open with a folder selected picks that folder; with
  nothing selected it picks the folder being looked at.
- Escape cancels. The Trash is not offered in the sidebar, because a chooser
  answers with things that exist.

Closing the window by any other route, the title-bar button included, answers
"cancelled": `PickerState` sends it on drop, so the caller's dialog is never
left blocked on a reply that will not come.

## Process lifetime

GPUI quits when the last window closes, and for a Marcel that
xdg-desktop-portal started on demand, the picker is the last window. The
reply is written to the bus from zbus's own thread, so the process could in
principle exit with the answer still in hand and the application would see
its backend vanish. `ReplyTracker` counts requests from the moment they are
decoded until the reply has left the process (zbus's
`ResponseDispatchNotifier` reports that), and a quit hook in `main.rs` waits
for the count to reach zero, up to 150 ms of the 200 ms GPUI allows. This was
confirmed by hand: a cold-started Marcel answers, then exits, and the caller
gets its reply.

## Packaging

`nix/file-chooser-portal.nix` is the variant that answers dialogs, built the
same way as the FileManager1 variant: the binary is wrapped to set
`MARCEL_CLAIM_FILE_CHOOSER=1`, which makes every launch claim the backend name
after it owns its own; a D-Bus activation file lets the frontend start Marcel
when a dialog is asked for while it is not running; and `marcel.portal` is
what makes the frontend consider Marcel at all. The name claim is an extra,
never a startup condition, exactly like `org.freedesktop.FileManager1`: if
another backend already owns it, Marcel says so on stderr and keeps browsing.

The flake exposes it as `packages.<system>.file-chooser-portal` and the
overlay as `marcelFileChooserPortal`. With the module it is one flag:

```nix
programs.marcel.fileChooserPortal = true;
```

which adds the configured package to `xdg.portal.extraPortals` and names
`marcel` for the FileChooser interface in `xdg.portal.config.common`.
`xdg.portal.enable` stays your responsibility. The three wrappers (FileManager1
claim, portal claim, settings) stack, and each one re-points every D-Bus
activation file underneath it at itself, so whichever combination is enabled,
a Marcel started by the bus is the fully configured one.

## Things that will bite

The frontend reads `portals.conf` and scans the portal files once, at
startup. After enabling Marcel, `systemctl --user restart xdg-desktop-portal`
or you keep getting the old dialog. Restarting it also D-Bus-activates every
configured backend straight away, to read their `version` properties, so a
Marcel with no windows appears on the bus at that point and stays resident.
That is the warm backend the dialog latency depends on, not a leak.

On a system running `dbus-broker` (NixOS does), activated services are
started through systemd and get no `DBUS_STARTER_*` variables. Marcel also
recognises the transient unit it lands in, `dbus-:1.4-<name>.service`, from
`/proc/self/cgroup`; before that check existed, every activated Marcel opened
a browsing window at the daemon's working directory.

Every launch of the *installed* binary claims the name, but a Marcel started
some other way does not. If a `cargo run` build is the running primary and the
frontend activates the installed one, the new process forwards its launch to
the primary and exits, the primary never took the portal name, and the
application gets an activation error. Close the development instance first.

Browsers are not uniform about using the portal. Firefox and Zen use it when
`widget.use-xdg-desktop-portal.file-picker` is `1`; the default `2` means
"only under GNOME or in a sandbox". Chromium-based browsers have switched to
the portal picker in recent releases, but if Helium keeps showing a GTK dialog
after the switch, that is the first thing to check rather than the backend.

## Checking it works without a browser

The frontend can be poked directly:

```sh
busctl --user call org.freedesktop.portal.Desktop \
  /org/freedesktop/portal/desktop \
  org.freedesktop.portal.FileChooser OpenFile 'ssa{sv}' '' 'Pick something' 0
```

That goes through the real routing. It also returns the moment the frontend
hands back a request handle, and the frontend cancels any request whose
caller has gone, so `busctl` alone never shows a dialog: the frontend logs
`Handling OpenFile` and drops it. Use it to check the routing (`xdg-desktop-portal
--verbose` prints which portal file it chose) and a real application to see
the dialog.
`busctl --user status org.freedesktop.impl.portal.desktop.marcel` says
whether Marcel is currently the one holding the name.

The backend can also be called past the frontend, which is how it was
exercised while being written. The method signature is `osssa{sv}` and the
call blocks until the window answers:

```sh
busctl --user --timeout=300 call \
  org.freedesktop.impl.portal.desktop.marcel \
  /org/freedesktop/portal/desktop \
  org.freedesktop.impl.portal.FileChooser OpenFile 'osssa{sv}' \
  /org/freedesktop/portal/desktop/request/manual/1 org.example.App '' \
  'Pick an image' 2 \
  filters 'a(sa(us))' 1 Images 2 0 '*.png' 0 '*.jpg' \
  multiple b true
```

`desktop::bus::tests::private_session_bus_child` covers the same
ground on a private bus: name ownership, the `version` property, a call
answered through the channel, `Close` withdrawing a pending call, a request
dropped without an answer, and the reply tracker draining afterwards.

## Prior art

[xdg-desktop-portal-termfilechooser](https://github.com/GermainZ/xdg-desktop-portal-termfilechooser)
is a backend that opens a terminal running yazi or ranger as the picker.
It is small, C, and covers exactly the three methods above with the
`Request` object and cancellation; a good reference for the D-Bus surface.
The interface itself is documented in the
[portal backend reference](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.impl.portal.FileChooser.html).
