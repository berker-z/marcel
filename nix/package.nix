{
  lib,
  callPackage,
  rustPlatform,
  craneLib ? null,
  copyDesktopItems,
  makeDesktopItem,
  makeWrapper,
  pkg-config,
  cmake,
  lld,
  appstream,
  desktop-file-utils,
  dbus,
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
  # Keep the standalone nixpkgs recipe available; flake builds use Crane so
  # dependencies survive application-only source changes.
  buildPackage =
    if craneLib == null then
      rustPlatform.buildRustPackage
    else
      argsFn:
      let
        args = argsFn (args // { finalPackage = package; });
        common = {
          inherit (args)
            pname
            version
            src
            nativeBuildInputs
            buildInputs
            RUSTFLAGS
            RUST_MIN_STACK
            ;
          cargoVendorDir = craneLib.vendorCargoDeps {
            cargoLock = args.cargoLock.lockFile;
            # Crane keys git hashes by the full Cargo source URL, whereas
            # importCargoLock keys them by one crate's name and version.
            outputHashes = builtins.listToAttrs (
              lib.concatMap (
                p:
                lib.optional (builtins.hasAttr "${p.name}-${p.version}" args.cargoLock.outputHashes) {
                  name = p.source;
                  value = args.cargoLock.outputHashes."${p.name}-${p.version}";
                }
              ) (builtins.fromTOML (builtins.readFile args.cargoLock.lockFile)).package
            );
            # Preserve importCargoLock's fetch mode for the existing hashes.
            # Crane otherwise enables Git LFS when fetching hashed sources.
            overrideVendorGitCheckout =
              _: drv:
              drv.overrideAttrs (old: {
                src = old.src.override { fetchLFS = false; };
              });
          };
        };
        cargoArtifacts = craneLib.buildDepsOnly common;
        package = craneLib.buildPackage (
          builtins.removeAttrs args [ "cargoLock" ]
          // common
          // {
            inherit cargoArtifacts;
            passthru = args.passthru // {
              inherit cargoArtifacts;
            };
          }
        );
      in
      package;
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
buildPackage (finalAttrs: {
  pname = "marcel-rs";
  version = "0.1.0";

  # rustc recommends increasing its worker-thread stack when LLVM exhausts the
  # default during code generation. Marcel's thin-LTO release build has
  # otherwise produced a nondeterministic libLLVM.so crash with LLVM 21, in
  # `gpui_linux` rather than in Marcel's own code.
  #
  # 16 MiB held for a while and then stopped: the same crash returned on
  # rustc 1.97.1 with LLVM 21.1.8, in `SimplifyCFGPass`. This is the size rustc
  # itself names in the failure, and it changes no optimization setting. If it
  # comes back again, the fix is not to keep doubling this in the dark: capture
  # which crate and which LLVM pass, because a stack that large usually means
  # one function is being inlined into something enormous.
  RUST_MIN_STACK = "33554432";

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

  # Link with LLD rather than the default `ld.bfd`.
  #
  # GNU ld fails this package's thin-LTO link with `.eh_frame_hdr refers to
  # overlapping FDEs`, a long-standing interaction between LTO output and
  # ld.bfd's exception-table merging rather than anything wrong with the
  # objects. LLD merges the same input without complaint, and pairing it with
  # an LLVM-produced LTO build is the more usual combination anyway.
  #
  # Turning thin-LTO off would also clear the error. That trades a real
  # optimization away to accommodate one linker, so the linker moves instead.
  RUSTFLAGS = "-C link-arg=-fuse-ld=lld";

  nativeBuildInputs = [
    rustPlatform.bindgenHook
    copyDesktopItems
    makeWrapper
    pkg-config
    cmake
    lld
  ];

  # The archive tests skip quietly when no 7zz is on PATH, which is the right
  # default on a developer machine and the wrong one here: a package built
  # without archive coverage would still report green. Put 7zz on the check
  # PATH and tell the suite to fail rather than skip without it.
  nativeCheckInputs = [
    dbus
    _7zz
  ];

  buildInputs = runtimeLibraries;

  preCheck = ''
    export HOME="$TMPDIR"
    export XDG_DATA_HOME="$TMPDIR/.local/share"
    export MARCEL_TEST_DBUS_SESSION_CONFIG=${./test-session.conf}
    export MARCEL_TEST_REQUIRE_7ZZ=1
    mkdir -p "$XDG_DATA_HOME/Trash/files" "$XDG_DATA_HOME/Trash/info"
  '';

  desktopItems = [
    (makeDesktopItem {
      name = "io.github.berker_z.Marcel";
      desktopName = "Marcel";
      genericName = "File Manager";
      comment = "Browse files with a fast, persistent preview pane";
      exec = "marcel-rs %U";
      dbusActivatable = true;
      icon = "io.github.berker_z.Marcel";
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
    # No MimeType here: the entry is hidden, but an association offer is not,
    # and with one on both entries some Open With lists showed Marcel twice.
    (makeDesktopItem {
      name = "marcel";
      desktopName = "Marcel";
      genericName = "File Manager";
      comment = "Compatibility desktop identifier for Marcel";
      exec = "marcel-rs %U";
      icon = "io.github.berker_z.Marcel";
      noDisplay = true;
    })
  ];

  postInstall = ''
    mkdir -p "$out/libexec/marcel"
    ln -s ${lib.getExe' _7zz "7zz"} "$out/libexec/marcel/7zz"

    mkdir -p "$out/share/marcel/icons" "$out/share/licenses/marcel" \
      "$out/share/icons"
    # Store paths are read-only; a plain `cp -R` carries that mode into
    # `$out`, and Crane's post-install hook then fails to `sed -i` the tree
    # while stripping toolchain references.
    cp -R --no-preserve=mode ${../assets/icons/nordzy} \
      "$out/share/marcel/icons/nordzy"
    cp -R --no-preserve=mode ${../assets/icons/hicolor} \
      "$out/share/icons/hicolor"
    # MIT wants its notice to travel with every copy, and the Yazi notice in
    # the third-party file is there for the same reason, so both ship next to
    # the asset licenses rather than staying behind in the repository.
    install -Dm644 ${../LICENSE} "$out/share/licenses/marcel/LICENSE"
    install -Dm644 ${../THIRD_PARTY_NOTICES.md} \
      "$out/share/licenses/marcel/THIRD_PARTY_NOTICES.md"
    install -Dm644 ${../assets/fonts/OFL-Iosevka.md} \
      "$out/share/licenses/marcel/OFL-Iosevka.md"
    install -Dm644 ${../assets/icons/nordzy/COPYING} \
      "$out/share/licenses/marcel/COPYING-Nordzy"

    install -Dm644 ${./io.github.berker_z.Marcel.metainfo.xml} \
      "$out/share/metainfo/io.github.berker_z.Marcel.metainfo.xml"

    mkdir -p "$out/share/dbus-1/services" "$out/share/dbus-1/interfaces"
    substitute ${./io.github.berker_z.Marcel.service} \
      "$out/share/dbus-1/services/io.github.berker_z.Marcel.service" \
      --replace-fail @marcel@ "$out"
    install -Dm644 ${./org.freedesktop.FileManager1.xml} \
      "$out/share/dbus-1/interfaces/org.freedesktop.FileManager1.xml"

    wrapProgram "$out/bin/marcel-rs" \
      --prefix PATH : ${
        lib.makeBinPath [
          glib
          poppler-utils
        ]
      } \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibraries}
  '';

  # Desktop metadata is only useful if it parses on the user's machine, and a
  # typo in it is invisible until a software centre silently ignores the
  # application. Validate what was installed, not what was written.
  #
  # `--no-net` keeps the screenshot URL from being fetched: the build sandbox
  # has no network, and an unreachable image would fail the build for a reason
  # that has nothing to do with the package.
  nativeInstallCheckInputs = [
    appstream
    desktop-file-utils
  ];

  doInstallCheck = true;

  installCheckPhase = ''
    runHook preInstallCheck

    appstreamcli validate --no-net --explain \
      "$out/share/metainfo/io.github.berker_z.Marcel.metainfo.xml"
    desktop-file-validate "$out/share/applications/"*.desktop

    runHook postInstallCheck
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
    changelog = "https://github.com/berker-z/marcel/blob/v${finalAttrs.version}/CHANGELOG.md";

    # Marcel's own code is MIT, but the package also installs the curated
    # Nordzy icon subset and the Iosevka subsets, which are not. nixpkgs uses a
    # list when parts of one package carry different licenses.
    license = with lib.licenses; [
      mit
      gpl3Only
      ofl
    ];

    mainProgram = "marcel-rs";
    platforms = lib.platforms.linux;
  };
})
