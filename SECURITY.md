# Security

Marcel is a file manager. It moves, copies, deletes, extracts, and changes the
permissions of the user's files, it answers requests from other programs over
D-Bus, and it hands files to external tools. A bug in any of that can destroy
data or let another program on the desktop do so through Marcel, which is why
this file exists before the first tag.

## Reporting

Use GitHub's private vulnerability reporting on
<https://github.com/berker-z/marcel/security/advisories/new>. It reaches the
maintainer, `berker-z`, and nobody else, and it gives the report a place to
live until a fix is out. If that route is not available to you, open an issue
that says only that you have a security report and how to contact you; do not
put the details in the issue.

Include what you would want yourself: the commit or version, what you did, what
happened, and what you expected. A file that triggers it, or a `busctl` line,
is worth more than a description of one.

## What counts

Anything that makes Marcel touch a file the user did not ask it to touch, or
touch it in a way they did not ask for. Concretely, the surfaces that matter:

- **Filesystem mutation** (`src/fsops/`). Every destructive step is supposed to
  check that the path still names the file it inspected, never follow a
  symbolic link it did not intend to, never overwrite, and hold replaced items
  aside until the operation is done. A race, a symlink, a hard link, or an
  extraction that gets past one of those checks is a vulnerability, whether or
  not another user on the machine is needed to exploit it.
- **Archive handling** (`src/fsops/archive.rs`). Extraction runs 7-Zip as a
  child process into a staging directory and publishes the result afterwards.
  An archive that writes outside the staging directory, follows a link out of
  it, exhausts disk or memory past the limits Marcel sets, or keeps running
  after Cancel is in scope.
- **The D-Bus and portal surface** (`src/desktop/`). Marcel owns
  `io.github.berker_z.Marcel`, optionally `org.freedesktop.FileManager1`, and
  optionally the `FileChooser` portal backend. Any process on the session bus
  can call these; sandboxed applications reach the portal through
  xdg-desktop-portal. A request that makes Marcel open a path it should not,
  crash, hang, answer a picker with a file the user did not choose, or write
  a saved file somewhere other than the folder the user picked is in scope.
- **External tools and previews** (`src/preview/`, `src/desktop/open.rs`,
  `src/desktop/terminal.rs`). Marcel runs `pdftoppm`, `pdfinfo`, `ffprobe`,
  `ffmpeg`, `7zz`, `gio`, and a terminal emulator. A browsed file name or
  content that ends up interpreted as an argument, a URL fetched during a
  preview, or a decoder that can be made to allocate without bound counts.

Out of scope: bugs in GPUI, 7-Zip, Poppler, or ffmpeg themselves (report those
upstream, though a note here is welcome if Marcel could mitigate), anything
that needs root or an already compromised user session, and denial of service
against Marcel's own window by the user sitting in front of it.

## What to expect

This is a one-person project maintained in spare time. You will get an
acknowledgement within a week, usually sooner, and an honest estimate once the
report is understood. A fix for something that can lose data gets a release on
its own; smaller things ride the next one. There is no bounty. Credit in the
changelog and the advisory is yours if you want it.

Only the latest release and `master` are supported. There are no backports.
