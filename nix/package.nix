{
  lib,
  callPackage,
  rustPlatform,
  craneLib ? null,
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
  libheif = callPackage ./libheif.nix { };

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

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../.cargo/config.toml
      ../Cargo.toml
      ../Cargo.lock
      ../LICENSE
      ../Makefile
      ../THIRD_PARTY_NOTICES.md
      ../assets
      # The PKGBUILD is not part of the build; leaving it out keeps a checksum
      # bump after a tag from rebuilding the Nix package.
      (lib.fileset.difference ../packaging ../packaging/arch)
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

  # `.cargo/config.toml` asks for LLD and a larger rustc stack; both are
  # there so a build outside Nix gets them too. LLD has to be on the path.
  nativeBuildInputs = [
    rustPlatform.bindgenHook
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

  buildInputs = runtimeLibraries ++ [ libheif ];

  preCheck = ''
    export HOME="$TMPDIR"
    export XDG_DATA_HOME="$TMPDIR/.local/share"
    export MARCEL_TEST_DBUS_SESSION_CONFIG=${../packaging/test-session.conf}
    export MARCEL_TEST_REQUIRE_7ZZ=1
    mkdir -p "$XDG_DATA_HOME/Trash/files" "$XDG_DATA_HOME/Trash/info"
  '';

  # Cargo has installed the binary; the `Makefile` places everything else, the
  # same way it does for every other package. The portal files are left to
  # the file-chooser variant, whose activation file has to start its own
  # wrapper. 7-Zip is linked in beside the binary, where Marcel looks first.
  postInstall = ''
    make install-data PREFIX="$out" PORTAL=0 \
      SEVENZIP=${lib.getExe' _7zz "7zz"}

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
