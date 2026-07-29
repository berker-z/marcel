{
  description = "Marcel, a fast preview-first graphical file explorer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      mkPkgs =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
          config.allowUnfreePredicate =
            package:
            builtins.elem (nixpkgs.lib.getName package) [
              "7zz"
              "uasm"
            ];
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = mkPkgs system;
          marcel = pkgs.callPackage ./nix/package.nix { };
        in
        {
          inherit marcel;
          default = marcel;
        }
      );

      apps = forAllSystems (system: {
        marcel = {
          type = "app";
          program = "${self.packages.${system}.marcel}/bin/marcel";
          meta.description = "Fast, preview-first graphical file explorer";
        };
        default = self.apps.${system}.marcel;
      });

      overlays.default = final: _previous: {
        marcel = final.callPackage ./nix/package.nix { };
      };

      checks = forAllSystems (system: {
        package = self.packages.${system}.marcel;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = mkPkgs system;

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
            libxcb
            libxcursor
            libxi
            libxrandr
          ];
        in
        {
          default = pkgs.mkShell {
            packages = [
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
        }
      );
    };
}
