{
  description = "Marcel, a fast preview-first graphical file explorer";

  nixConfig = {
    extra-substituters = [ "https://marcel-rs.cachix.org" ];
    extra-trusted-public-keys = [
      "marcel-rs.cachix.org-1:ae3s4u7pctzohvoTn8DWdMnRCrLEg1u32OIjfQ7p0VY="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";

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
      crane,
      ...
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      mkRustToolchain =
        pkgs:
        pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "clippy"
            "rust-src"
            "rustfmt"
          ];
        };
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
          marcel = pkgs.callPackage ./nix/package.nix {
            craneLib = (crane.mkLib pkgs).overrideToolchain (mkRustToolchain pkgs);
          };
          marcelFileManager1Service = pkgs.callPackage ./nix/file-manager1-service.nix {
            inherit marcel;
          };
          marcelFileChooserPortal = pkgs.callPackage ./nix/file-chooser-portal.nix {
            inherit marcel;
          };
        in
        {
          # `marcel-rs`, not `marcel`: nixpkgs already has an unrelated
          # `marcel`, and an overlay that binds that name would replace it for
          # every user of this flake rather than adding to it.
          marcel-rs = marcel;
          marcel-deps = marcel.cargoArtifacts;
          file-manager1-service = marcelFileManager1Service;
          file-chooser-portal = marcelFileChooserPortal;
          default = marcel;
        }
      );

      apps = forAllSystems (system: {
        marcel-rs = {
          type = "app";
          program = "${self.packages.${system}.marcel-rs}/bin/marcel-rs";
          meta.description = "Fast, preview-first graphical file explorer";
        };
        default = self.apps.${system}.marcel-rs;
      });

      overlays.default =
        final: _previous:
        let
          marcel = final.callPackage ./nix/package.nix {
            craneLib = (crane.mkLib final).overrideToolchain (
              mkRustToolchain (mkPkgs final.stdenv.hostPlatform.system)
            );
          };
        in
        {
          # Binding `marcel` here would shadow nixpkgs' own `marcel`, an
          # unrelated Python shell, for anyone who applies this overlay. An
          # overlay should add a package, not quietly replace someone else's.
          marcel-rs = marcel;
          marcelFileManager1Service = final.callPackage ./nix/file-manager1-service.nix {
            inherit marcel;
          };
          marcelFileChooserPortal = final.callPackage ./nix/file-chooser-portal.nix {
            inherit marcel;
          };
        };

      homeManagerModules.default = import ./nix/settings-module.nix {
        flake = self;
        integration = "home-manager";
      };

      nixosModules.default = import ./nix/settings-module.nix {
        flake = self;
        integration = "nixos";
      };

      checks = forAllSystems (system: {
        package = self.packages.${system}.marcel-rs;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = mkPkgs system;

          rustToolchain = mkRustToolchain pkgs;

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
              appstream
              clang
              cmake
              dbus
              desktop-file-utils
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

            # Local Cargo builds run outside nix-daemon, so the daemon's job
            # and memory limits do not apply to them.
            CARGO_BUILD_JOBS = "2";

            # `desktop_integration`'s integration test starts a private session
            # bus, and it has to be a bus with no system configuration behind
            # it. See the comment in `nix/test-session.conf`.
            # Interpolated, not `toString`. `toString` yields the path as a
            # string without making the file a dependency of the shell, so the
            # name it produces is one nothing guarantees exists: on a hosted
            # runner it pointed into a flake source path that was never there,
            # and dbus-daemon failed to open it. Interpolating copies the file
            # into the store and depends on it.
            MARCEL_TEST_DBUS_SESSION_CONFIG = "${./nix/test-session.conf}";
          };
        }
      );
    };
}
