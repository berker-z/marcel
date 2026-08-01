# Sprint 16: public release presentation and metadata

**Status:** Planned

## Goal

Turn Marcel's working personal daily-driver build into an honest, attractive,
and maintainable public release candidate without pretending unsupported
distribution formats already exist.

## Documentation contract

- The root README is product-first and quickly explains what Marcel is, how it
  looks, what is safe to try, and which installation routes actually exist.
- Installation structure is platform-neutral. Nix is marked supported today;
  future AppImage, Flatpak, AUR, Debian/Ubuntu, and Fedora/RPM sections remain
  clearly unavailable until real artifacts exist.
- Installation never silently means “make Marcel the default.” MIME association
  and generic FileManager1 ownership remain separate explicit steps.
- Maintainer release mechanics stay in `docs/release.md`; the README links to
  them rather than duplicating the handbook.

## Visual presentation contract

- Add an original Marcel application icon in scalable and required raster
  hicolor sizes and use the reverse-DNS icon name consistently.
- Add a short optimized GIF demonstrating representative navigation, filtering,
  previews, and a safe interaction. It must contain no personal data, include
  useful alt text, and have a static fallback.
- Add a small, curated screenshot set that demonstrates the deliberate default
  identity without turning the repository into an image archive.

## Package metadata contract

- Install and validate `io.github.berker_z.Marcel.metainfo.xml`.
- Keep application ID, desktop ID, icon name, D-Bus service, launchable
  metadata, project URLs, license, content rating, and release version aligned.
- Describe the free archive baseline and private bundled visual resources
  accurately.

## Acceptance checks

- [ ] Rewrite and review the root README against the documentation backlog.
- [ ] Add the hero GIF, static fallback, alt text, and reproducible capture
  notes.
- [ ] Add representative screenshots with intentional filenames and sizes.
- [ ] Add the original branded icon in scalable and required raster sizes.
- [ ] Install and validate complete AppStream metadata.
- [ ] Add a platform support/install matrix with no fictional commands.
- [ ] Separate install, default-handler, and generic FileManager1 instructions.
- [ ] Add a contributor quickstart and automated Markdown-link checking.
- [ ] Add mandatory hosted checks for `cargo fmt --check`, Clippy with warnings
  denied, all-target tests, and the declared Nix package build.
- [ ] Audit all current docs for stale feature, limitation, sprint, version,
  and pinned-commit claims.
- [ ] Run the release-only Nix build/check and clean installed-package smoke
  test after the release commit is assembled.
