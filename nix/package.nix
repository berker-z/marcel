{
  lib,
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
  _7zz-rar,
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
rustPlatform.buildRustPackage {
  pname = "marcel";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

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
      name = "marcel";
      desktopName = "Marcel";
      genericName = "File Manager";
      comment = "Browse files with a fast, persistent preview pane";
      exec = "marcel %U";
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
  ];

  postInstall = ''
    mkdir -p "$out/libexec/marcel"
    ln -s ${lib.getExe' _7zz-rar "7zz"} "$out/libexec/marcel/7zz"

    wrapProgram "$out/bin/marcel" \
      --prefix PATH : ${
        lib.makeBinPath [
          glib
          poppler-utils
        ]
      } \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibraries}
  '';

  meta = {
    description = "Fast, preview-first graphical file explorer";
    homepage = "https://github.com/berker-z/marcel";
    license = lib.licenses.mit;
    mainProgram = "marcel";
    platforms = lib.platforms.linux;
  };
}
