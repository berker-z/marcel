# Sprint 14: desktop interoperability and single-instance routing

**Status:** Implemented core — branded D-Bus activation, single-instance
routing, packaging, and bilateral Wayland file drag-and-drop are delivered.
Private-bus/manual acceptance remains; Properties and X11 outbound drag are
parked follow-ups.

## Goal

Make Marcel behave like a native Linux file manager when launched repeatedly
or contacted by desktop software, while keeping installation separate from the
user's decision to make Marcel the generic/default file manager.

This sprint covers process routing, the standard D-Bus entry points, and native
local-file drag-and-drop needed for browser and desktop interoperability.
Desktop clipboard exchange remains a later, separate slice.

## Native file drag-and-drop

- Accept bounded local-file drops from browsers, desktop shells, and other
  file managers onto Places, bookmarks, directory entries, and empty browser
  space.
- External drops are copy operations. They never delete or rename the source,
  never overwrite an existing destination, and use Marcel's normal cancellable
  transfer controller.
- Reject empty, oversized, relative, self, same-parent, and recursive
  descendant drops before scheduling work.
- Export selected local files as `text/uri-list` through the display server so
  native Wayland applications, including browsers, can receive them.
- Preserve Marcel's existing move behavior when the native drag returns to a
  Marcel-owned drop target.
- Marcel uses GPUI's upstream external-file-drag lifecycle. On Wayland the
  source advertises the standard Copy-or-Move action set and the target chooses
  the result; X11 source support remains an explicit follow-up. X11 and Wayland
  inbound drops both use GPUI's existing support.

## Standards and ownership contract

- Choose one stable reverse-DNS application ID and use it consistently for the
  branded desktop entry, D-Bus service name, object path, and package metadata.
- Implement the session-bus `org.freedesktop.Application` methods `Activate`,
  `Open`, and `ActivateAction` at the object path derived from that ID.
- Implement `org.freedesktop.FileManager1` at
  `/org/freedesktop/FileManager1`, with `ShowFolders`, `ShowItems`, and
  `ShowItemProperties`.
- One Marcel process owns the application-specific name in a graphical login
  session. Later CLI or desktop launches forward their request to that process
  and exit successfully instead of opening an unrelated second coordinator.
- Installing `pkgs.marcel` must not by itself claim or replace the generic
  `org.freedesktop.FileManager1` activation service. The flake must expose an
  explicit downstream opt-in for generic ownership, just as MIME default
  selection remains explicit.
- Keep an `Exec=` fallback in the desktop entry even when D-Bus activation is
  enabled, as required for desktops that do not support it.

## Request behavior

- `Activate` presents an existing Marcel window or opens the normal starting
  location when no window exists.
- `Open` accepts local file URIs. Directory targets open normally; regular
  files open their parent and are revealed using Marcel's existing location
  resolver and navigation path.
- `ShowFolders` accepts only local URIs that resolve to directories and shows
  their contents. It must never reinterpret a regular file as something to
  execute or open with its associated application.
- `ShowItems` accepts local file or directory URIs, groups targets by parent
  directory where appropriate, opens the required locations, and selects or
  reveals the requested items.
- `ShowItemProperties` opens Marcel's Properties presentation for valid local
  targets. This method must share the same metadata model as the in-app
  Properties command rather than growing a second implementation.
- Requests involving multiple locations have deterministic window and focus
  behavior. A request must not silently discard every URI after the first.
- Activation tokens or startup IDs are treated as opaque presentation hints.
  Marcel never evaluates or passes them through a shell.

## Safety and concurrency contract

- Treat D-Bus and forwarded CLI input as untrusted. Bound URI count and total
  input size, accept only local `file:` URIs where the standard requires URIs,
  and return a typed D-Bus error for invalid, remote, oversized, or
  type-mismatched requests.
- Validate the complete request before publishing any window or navigation
  change, so one malformed URI cannot leave a partially applied batch.
- Perform URI decoding, filesystem metadata, canonicalization, and grouping
  off GPUI's foreground executor.
- Deliver validated requests to GPUI through a bounded channel or equivalent
  Marcel-owned interface. A disconnected window or superseded request must not
  publish stale state.
- D-Bus work must not mutate files. It may only activate windows, navigate,
  reveal selections, or show read-only properties.
- A primary-instance crash must release its bus names normally; a later launch
  must recover without stale lock files.

## Packaging and migration

- Install the application-specific D-Bus service and introspection metadata in
  the Nix package.
- Preserve a compatibility path for the existing `marcel.desktop` identifier
  if the branded reverse-DNS desktop filename changes, without leaving two
  visible launcher entries.
- Expose and document an explicit NixOS/Home Manager opt-in for the generic
  `org.freedesktop.FileManager1` service.
- Keep directory MIME association opt-in and continue claiming no archive MIME
  types.
- Document how to test the branded service directly even while another file
  manager owns the generic service.

## Acceptance checks

- [x] Add unit tests for request validation, URI limits, type checks, grouping,
  and application-ID/object-path derivation.
- [ ] Add integration tests against a private session bus for name ownership,
  activation, forwarding, typed errors, and primary-process exit recovery.
- [x] Confirm a second `marcel PATH` forwards to the existing process and
  reveals the requested target.
- [ ] Confirm application-specific `Activate` and `Open` work from a cold and
  warm process.
- [x] Confirm `ShowFolders` rejects regular files, remote URIs, and malformed
  batches without opening or executing anything.
- [ ] Confirm `ShowItems` reveals files requested by a browser-style “Show in
  folder” call.
- [ ] Confirm `ShowItemProperties` and the in-app Properties command share one
  read-only implementation.
- [ ] Confirm installing the ordinary package leaves the current generic file
  manager service and MIME defaults untouched.
- [ ] Confirm the explicit generic-service opt-in routes
  `org.freedesktop.FileManager1` calls to Marcel.
- [ ] Test both Wayland and X11 activation/focus behavior, including startup ID
  and activation-token handling where the compositor supports it.
- [x] Add bounded external-drop validation tests and route accepted drops
  through the normal no-overwrite copy controller.
- [x] Manually confirm Chrome-to-Marcel and Marcel-to-Chrome file drags on
  Wayland.
- [ ] Add and manually confirm the native file-drag source on X11.
- [x] Run the Rust quality gate.
- [x] For the release commit, build the real Nix package and run
  `nix flake check`.

## References

- [File-manager D-Bus interface][file-manager-interface]
- [Desktop Entry Specification: D-Bus activation][desktop-dbus]
- [D-Bus Specification: bus names and service activation][dbus-spec]

[file-manager-interface]: https://www.freedesktop.org/wiki/Specifications/file-manager-interface/
[desktop-dbus]: https://specifications.freedesktop.org/desktop-entry/latest/dbus.html
[dbus-spec]: https://dbus.freedesktop.org/doc/dbus-specification.html
