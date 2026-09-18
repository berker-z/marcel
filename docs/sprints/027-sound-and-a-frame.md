# Sprint 27: Sound in the pane, and one frame of a video

**Status:** Implemented. The quality gate is green. Driven by hand in a
running build on real files: an MP3 with cover art, a FLAC, a WhatsApp
voice note, and a handful of videos; play, pause, seek, and end of file.

## Goal

The preview pane could show a text file, an image, a PDF, and a folder, and
said "no preview" to everything people actually keep in Downloads: music,
voice notes, and clips. This sprint makes audio play where it is, gives
video a frame and a way to open it, and does it without making the package
larger by default.

## What was built

### Audio

`preview/audio.rs` opens a file with symphonia (MP3, FLAC, Ogg Vorbis, WAV,
AAC, ALAC; the decoders are compiled in) and yields interleaved `f32`. The
same `AudioSource` serves the one pass that measures the waveform (240 peak
buckets, cached under `~/.cache/marcel/waveform-v1/` by file identity, so a
track is decoded once) and the player, which reads at listening speed on a
thread of its own.

`preview/player.rs` is that thread. It starts on the first Play, not when
the preview loads, and blocks on its command channel while paused so an
idle preview costs nothing. Decoded chunks go to the sound card over a
bounded `sync_channel`; the card's callback copies from the chunk it holds
and never takes a lock, and a seek bumps a generation number so chunks
already queued are dropped unplayed. Pause pauses the card's stream, so
resume continues from the same sample. Position is what the card has
consumed, not what was decoded. The spectrum is a Hann-windowed 1024-point
FFT of the last samples played, in 48 log-spaced bands from 40 Hz to
16 kHz, in decibels, with a fast rise and a slow fall. Linear resampling
covers the difference between the file's rate and the card's.

Symphonia has no Opus decoder, and every voice note is Opus in Ogg. When
symphonia refuses a codec and ffmpeg is on `PATH`, the source is a child
`ffmpeg` writing raw `f32` to a pipe instead: same player, same bars, and a
seek is a fresh child at `-ss`. Tags come from `ffprobe` then; an Ogg keeps
them on the stream, not the container.

`app/media_pane.rs` draws it: cover or a placeholder, title and by-line,
the bars, the waveform as a scrubber (a press anywhere seeks to that
fraction), play/pause, the clock, and the stream facts. While the player
runs, a foreground task repaints twenty times a second and stops a second
after the sound does, so the bars settle.

### Video

`preview/media.rs` runs `ffprobe` for duration, size, codecs, and tags, and
`ffmpeg` for one frame ten percent in (capped at a minute, first frame for
anything under ten seconds), as a JPEG in `~/.cache/marcel/video-v1/` by
identity. The pane shows the frame with a round play button over it that
goes through `open_entry`, the same path as Enter, so the file opens in
whatever the desktop plays video with. Marcel does not play video itself;
see the backlog for what that would cost.

The grid takes its video thumbnails from the same frame (three seconds in,
no probe first), through the existing resize-and-write path, so the result
lands in the freedesktop cache with the right `Thumb::URI` and every other
application benefits. A tile with a thumbnail carries a play mark in the
middle, so a folder of clips and photos reads at a glance. A video
thumbnail another application already made shows whether or not ffmpeg is
installed.

### ffmpeg

Not bundled. `ffmpeg-headless` is a 302 MiB closure on this system's
nixpkgs against Marcel's 224, and only the two video paths and Opus need
it. `media::available()` looks for `ffprobe` and `ffmpeg` on `PATH` per
preview, the pane says "Video preview needs ffmpeg on PATH" when they are
missing, and the Nix module's `settings.media = true` wraps ffmpeg-headless
onto the binary's `PATH` for people who want it guaranteed.

The subprocess rules the PDF bridge had (a deadline, the preview's
cancellation flag, bounded output, failure worded from stderr, a private
cache pruned by age) moved to `preview/tool.rs`, and `pdf.rs` uses them from
there.

### Properties

`Details::Audio` (title, artist, album, duration, codec, rate, channels) and
`Details::Video` (duration, dimensions, streams) appear as rows.

## Not built

Volume; the system mixer has one. Playlists or continuing into the next
file. Video playback. An Opus decoder of Marcel's own. The media pane is
still part of the window view, so its repaints re-render the whole window
(on the backlog).

## Acceptance checks

- `cargo fmt --check && cargo clippy --all-targets --all-features -- -D
  warnings && cargo test --all-targets` passes, with
  `desktop::bus::tests::private_session_bus_integration` run unsandboxed.
  The Opus test skips itself where ffmpeg is absent, so the package's check
  phase is unaffected.
- [x] An MP3 with cover art shows the cover, title, artist and album; Play
  lights the bars, the clock runs, a click on the waveform seeks.
- [x] A FLAC without tags shows the placeholder and the file name.
- [x] A WhatsApp `.ogg` (Opus) plays through ffmpeg with its title.
- [x] Pause and resume continue from the same place.
- [x] A video shows a frame and the facts line; the play button opens it in
  the default player; its grid tile has a thumbnail and the play mark.
- [x] Selecting the same track again reads the waveform from the cache
  (no decode delay the second time).
- [ ] With ffmpeg off `PATH`: a video says it needs ffmpeg, an Opus file
  falls back to the summary, and a video thumbnail another application made
  still shows.
- [ ] `settings.media = true` in the module puts ffmpeg on the wrapper's
  PATH (`nix build` of the configured package, then `grep PATH result/bin/marcel-rs`).
- [ ] A track played to the end and played again starts from the top.
