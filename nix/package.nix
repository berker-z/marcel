{
  lib,
  callPackage,
  rustPlatform,
  copyDesktopItems,
  makeDesktopItem,
  makeWrapper,
  pkg-config,
  cmake,
  alsa-lib,
  expat,
  fontconfig,
  freetype,
  glib,
  libGL,
  libxkbcommon,
  vulkan-loader,
  wayland,
  libx11,
  libxcb,
  libxcursor,
  libxi,
  libxrandr,
  poppler-utils,
  _7zz,
}:
let
  runtimeLibraries = [
    alsa-lib
    expat
    fontconfig
    freetype
    libGL
    libxkbcommon
    vulkan-loader
    wayland
    libx11
    libxcb
    libxcursor
    libxi
    libxrandr
  ];
in
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "marcel";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../assets
      ../src
    ];
  };
  cargoLock = {
    lockFile = ../Cargo.lock;
    outputHashes = {
      "collections-0.1.0" = "sha256-S4jQkfcy0n0pIEQ66RfTtplFaU0DoCCB+OxVIq9Fo08=";
      "gpui-component-0.5.2" = "sha256-5ZCa7TNd+s37BZaD+QtmekvSNTbnZprENMv43QtTqqA=";
      "wasm_thread-0.3.3" = "sha256-+lRLCIk0S6Y5ORYjDKsYYHia2FtoSoh+rWkQh7mnPBE=";
      "xim-ctext-0.3.0" = "sha256-pRT4Sz1JU9ros47/7pmIW9kosWOGMOItcnNd+VrvnpE=";
      "zed-font-kit-0.14.1-zed" = "sha256-KXygi0olNQi5yM8eaJVykNDtbPMDjT+cWPBF8UrtXR4=";
      "zed-reqwest-0.12.15-zed" = "sha256-p4SiUrOrbTlk/3bBrzN/mq/t+1Gzy2ot4nso6w6S+F8=";
      "zed-scap-0.0.8-zed" = "sha256-BihiQHlal/eRsktyf0GI3aSWsUCW7WcICMsC2Xvb7kw=";
    };
  };

  nativeBuildInputs = [
    rustPlatform.bindgenHook
    copyDesktopItems
    makeWrapper
    pkg-config
    cmake
  ];

  buildInputs = runtimeLibraries;

  preCheck = ''
    export HOME="$TMPDIR"
    export XDG_DATA_HOME="$TMPDIR/.local/share"
    mkdir -p "$XDG_DATA_HOME/Trash/files" "$XDG_DATA_HOME/Trash/info"
  '';

  desktopItems = [
    (makeDesktopItem {
      name = "io.github.berker_z.Marcel";
      desktopName = "Marcel";
      genericName = "File Manager";
      comment = "Browse files with a fast, persistent preview pane";
      exec = "marcel %U";
      dbusActivatable = true;
      icon = "system-file-manager";
      categories = [
        "System"
        "FileTools"
        "FileManager"
      ];
      mimeTypes = [ "inode/directory" ];
      keywords = [
        "files"
        "folders"
        "explorer"
        "manager"
      ];
    })
    (makeDesktopItem {
      name = "marcel";
      desktopName = "Marcel";
      genericName = "File Manager";
      comment = "Compatibility desktop identifier for Marcel";
      exec = "marcel %U";
      icon = "system-file-manager";
      noDisplay = true;
      mimeTypes = [ "inode/directory" ];
    })
  ];

  postInstall = ''
    mkdir -p "$out/libexec/marcel"
    ln -s ${lib.getExe' _7zz "7zz"} "$out/libexec/marcel/7zz"

    mkdir -p "$out/share/marcel/icons" "$out/share/licenses/marcel"
    cp -R ${../assets/icons/nordzy} "$out/share/marcel/icons/nordzy"
    install -Dm644 ${../assets/fonts/OFL-Iosevka.md} \
      "$out/share/licenses/marcel/OFL-Iosevka.md"
    install -Dm644 ${../assets/icons/nordzy/COPYING} \
      "$out/share/licenses/marcel/COPYING-Nordzy"

    mkdir -p "$out/share/dbus-1/services" "$out/share/dbus-1/interfaces"
    substitute ${./io.github.berker_z.Marcel.service} \
      "$out/share/dbus-1/services/io.github.berker_z.Marcel.service" \
      --replace-fail @marcel@ "$out"
    install -Dm644 ${./org.freedesktop.FileManager1.xml} \
      "$out/share/dbus-1/interfaces/org.freedesktop.FileManager1.xml"

    wrapProgram "$out/bin/marcel" \
      --prefix PATH : ${
        lib.makeBinPath [
          glib
          poppler-utils
        ]
      } \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibraries}
  '';

  passthru.withSettings =
    settings:
    callPackage ./configured-package.nix {
      marcel = finalAttrs.finalPackage;
      inherit settings;
    };

  meta = {
    description = "Fast, preview-first graphical file explorer";
    homepage = "https://github.com/berker-z/marcel";
    license = lib.licenses.mit;
    mainProgram = "marcel";
    platforms = lib.platforms.linux;
  };
})
