{
  description = "Marcel file explorer development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    rust-overlay,
    ...
  }: let
    supportedSystems = [
      "x86_64-linux"
      "aarch64-linux"
    ];

    forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
  in {
    devShells = forAllSystems (system: let
      overlays = [rust-overlay.overlays.default];
      pkgs = import nixpkgs {
        inherit system overlays;
        config.allowUnfreePredicate = package:
          builtins.elem (nixpkgs.lib.getName package) [
            "7zz"
            "uasm"
          ];
      };

      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        extensions = [
          "clippy"
          "rust-src"
          "rustfmt"
        ];
      };

      runtimeLibraries = with pkgs; [
        alsa-lib
        expat
        fontconfig
        freetype
        libGL
        libxkbcommon
        vulkan-loader
        wayland
        libx11
        libxcursor
        libxi
        libxrandr
      ];
    in {
      default = pkgs.mkShell {
        packages =
          [
            rustToolchain
          ]
          ++ (with pkgs; [
            clang
            cmake
            ffmpeg
            file
            git
            pkg-config
            poppler-utils
            _7zz-rar
          ])
          ++ runtimeLibraries;

        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibraries;
        RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
      };
    });
  };
}
