#!/usr/bin/env python3
"""Rebuild Marcel's pinned, private font and icon identity bundle."""

from __future__ import annotations

import argparse
import hashlib
import shutil
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

from fontTools import subset
from fontTools.ttLib import TTFont


IOSEVKA_VERSION = "34.8.0"
NORDZY_VERSION = "1.8.7"
IOSEVKA_ARCHIVE = (
    f"https://github.com/be5invis/Iosevka/releases/download/v{IOSEVKA_VERSION}/"
    f"PkgTTF-Iosevka-{IOSEVKA_VERSION}.zip"
)
IOSEVKA_LICENSE = (
    f"https://raw.githubusercontent.com/be5invis/Iosevka/v{IOSEVKA_VERSION}/LICENSE.md"
)
NORDZY_ARCHIVE = (
    f"https://github.com/alvatip/Nordzy-icon/archive/refs/tags/{NORDZY_VERSION}.tar.gz"
)

DOWNLOADS = {
    "iosevka.zip": (
        IOSEVKA_ARCHIVE,
        "882dd68d3b2f1ad0e44b094d869b261d09ce17cc29818a28a19d60970483247f",
    ),
    "iosevka-license.md": (
        IOSEVKA_LICENSE,
        "4ba53c7c1cb39279aae5f8d7d22054c485c71169920e5a36ed098b115e2e3c5d",
    ),
    "nordzy.tar.gz": (
        NORDZY_ARCHIVE,
        "e3bdef7ce74bcbebc0e5c74481bc4d6ead022cee31b99aeb3e54fc26a5ef9bb5",
    ),
}

UNICODES = (
    "U+0000-024F,U+0370-052F,U+2000-206F,U+20A0-20CF,"
    "U+2190-22FF,U+2500-26FF,U+FB00-FB06"
)

NORDZY_ICONS = {
    "folder": ("places", "folder"),
    "user-home": ("places", "user-home"),
    "user-desktop": ("places", "user-desktop"),
    "folder-documents": ("places", "folder-documents"),
    "folder-download": ("places", "folder-download"),
    "folder-music": ("places", "folder-music"),
    "folder-pictures": ("places", "folder-images"),
    "folder-publicshare": ("places", "folder-public"),
    "folder-templates": ("places", "folder-templates"),
    "folder-videos": ("places", "folder-videos"),
    "user-trash": ("places", "user-trash"),
    "user-trash-full": ("places", "user-trash-full"),
    "application-pdf": ("mimes", "application-pdf"),
    "package-x-generic": ("mimes", "package-x-generic"),
    "text-x-generic": ("mimes", "text-x-generic"),
    "image-x-generic": ("mimes", "image-x-generic"),
    "audio-x-generic": ("mimes", "audio-x-generic"),
    "video-x-generic": ("mimes", "video-x-generic"),
    "application-x-generic": ("mimes", "text-x-preview"),
    "unknown": ("mimes", "unknown"),
}


def digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def fetch(cache: Path, filename: str) -> Path:
    url, expected = DOWNLOADS[filename]
    destination = cache / filename
    if not destination.exists() or digest(destination) != expected:
        cache.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=cache, delete=False) as temporary:
            temporary_path = Path(temporary.name)
        try:
            print(f"Downloading {url}")
            urllib.request.urlretrieve(url, temporary_path)
            if digest(temporary_path) != expected:
                raise RuntimeError(f"SHA-256 mismatch for {url}")
            temporary_path.replace(destination)
        finally:
            temporary_path.unlink(missing_ok=True)
    return destination


def rename_font(font: TTFont, style: str) -> None:
    names = {
        1: "Marcel Iosevka",
        2: style,
        3: f"Marcel Iosevka {style} {IOSEVKA_VERSION}",
        4: f"Marcel Iosevka {style}",
        6: f"MarcelIosevka-{style}",
        16: "Marcel Iosevka",
        17: style,
        21: "Marcel Iosevka",
        22: style,
    }
    table = font["name"]
    records = {(name.platformID, name.platEncID, name.langID) for name in table.names}
    for name_id, value in names.items():
        for platform_id, encoding_id, language_id in records:
            try:
                table.setName(value, name_id, platform_id, encoding_id, language_id)
            except UnicodeEncodeError:
                continue


def build_fonts(archive: Path, license_path: Path, output: Path) -> None:
    output.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(archive) as source:
        for style in ("Regular", "SemiBold"):
            with source.open(f"Iosevka-{style}.ttf") as font_source:
                font = TTFont(font_source, recalcTimestamp=False)
            options = subset.Options()
            options.recalc_timestamp = False
            subsetter = subset.Subsetter(options=options)
            subsetter.populate(unicodes=subset.parse_unicodes(UNICODES))
            subsetter.subset(font)
            rename_font(font, style)
            font["head"].modified = 0
            font.save(output / f"MarcelIosevka-{style}.ttf", reorderTables=True)
    license_text = license_path.read_text(encoding="utf-8")
    normalized_license = "\n".join(line.rstrip() for line in license_text.splitlines()) + "\n"
    (output / "OFL-Iosevka.md").write_text(normalized_license, encoding="utf-8")


def build_icons(archive: Path, output: Path) -> None:
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with tarfile.open(archive) as source:
            source.extractall(root, filter="data")
        source_root = next(root.glob("Nordzy-icon-*"))
        for destination_name, (category, source_name) in NORDZY_ICONS.items():
            source = source_root / "src" / category / "scalable" / f"{source_name}.svg"
            if not source.is_file():
                raise RuntimeError(f"missing Nordzy source icon: {source}")
            shutil.copyfile(source, output / f"{destination_name}.svg")
        shutil.copyfile(source_root / "COPYING", output / "COPYING")


def write_manifest(output_root: Path) -> None:
    (output_root / "README.md").write_text(
        f"""# Marcel identity assets

These private application resources are reproducibly generated by
`scripts/build_identity_assets.py`.

- Iosevka {IOSEVKA_VERSION}: regular and semibold TTF faces subset to
  `{UNICODES}` and renamed to `Marcel Iosevka`. Licensed under SIL OFL-1.1.
  Source: {IOSEVKA_ARCHIVE}
- Nordzy {NORDZY_VERSION}: twenty unmodified semantic SVG icons selected from
  the upstream theme. Licensed under GPL-3.0-only. Source: {NORDZY_ARCHIVE}

The font is embedded in Marcel rather than installed in the user's font
registry. The icons form a private fallback and are not installed as a
system-wide freedesktop theme. Source archive hashes are pinned in the
generator.
""",
        encoding="utf-8",
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--cache-dir",
        type=Path,
        default=Path(".cache/marcel-identity"),
        help="directory for verified upstream downloads",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("assets"),
        help="asset output directory",
    )
    arguments = parser.parse_args()

    iosevka_archive = fetch(arguments.cache_dir, "iosevka.zip")
    iosevka_license = fetch(arguments.cache_dir, "iosevka-license.md")
    nordzy_archive = fetch(arguments.cache_dir, "nordzy.tar.gz")
    build_fonts(iosevka_archive, iosevka_license, arguments.output / "fonts")
    build_icons(nordzy_archive, arguments.output / "icons" / "nordzy")
    write_manifest(arguments.output)


if __name__ == "__main__":
    main()
