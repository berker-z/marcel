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
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = mkPkgs system;
          marcel = pkgs.callPackage ./nix/package.nix { };
          marcelFileManager1Service = pkgs.callPackage ./nix/file-manager1-service.nix {
            inherit marcel;
          };
        in
        {
          inherit marcel;
          file-manager1-service = marcelFileManager1Service;
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

      overlays.default =
        final: _previous:
        let
          marcel = final.callPackage ./nix/package.nix { };
        in
        {
          inherit marcel;
          marcelFileManager1Service = final.callPackage ./nix/file-manager1-service.nix {
            inherit marcel;
          };
        };

      homeManagerModules.default = import ./nix/settings-module.nix {
        flake = self;
        packageOption = [
          "home"
          "packages"
        ];
      };

      nixosModules.default = import ./nix/settings-module.nix {
        flake = self;
        packageOption = [
          "environment"
          "systemPackages"
        ];
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
              _7zz
            ])
            ++ runtimeLibraries;

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibraries;
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          };
        }
      );
    };
}
