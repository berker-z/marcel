{
  flake,
  # "home-manager" or "nixos": the two module systems install packages and
  # register MIME defaults under different option paths.
  integration,
}:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.marcel;
  system = pkgs.stdenv.hostPlatform.system;
  isHome = integration == "home-manager";

  # Layer the wrappers: the generic-name claim goes under the settings
  # wrapper, so the binary the module installs, activates over D-Bus, and
  # names in the override below is the same fully configured one.
  basePackage =
    if cfg.fileManager1 then
      pkgs.callPackage ./file-manager1-service.nix { marcel = cfg.package; }
    else
      cfg.package;
  configuredPackage = pkgs.callPackage ./configured-package.nix {
    marcel = basePackage;
    inherit (cfg) settings;
  };

  fileManager1Service = ''
    [D-BUS Service]
    Name=org.freedesktop.FileManager1
    Exec=${configuredPackage}/bin/marcel-rs
  '';
in
{
  options.programs.marcel = {
    enable = lib.mkEnableOption "Marcel file explorer";

    package = lib.mkOption {
      type = lib.types.package;
      default = flake.packages.${system}.marcel-rs;
      defaultText = lib.literalExpression "inputs.marcel.packages.${pkgs.system}.marcel-rs";
      description = "The Marcel package to configure and install.";
    };

    defaultDirectoryHandler = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Register Marcel as the default application for `inode/directory`,
        so `xdg-open` on a folder, and "open containing folder" in
        applications that go through it, open Marcel.
      '';
    };

    fileManager1 = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Have Marcel answer `org.freedesktop.FileManager1` on the session
        bus, which is what browsers and most other applications call for
        "show in folder". Every launch of the installed binary then claims
        the name, and the Home Manager module also writes a D-Bus activation
        file to `~/.local/share/dbus-1/services`, which D-Bus reads before
        any installed package's, so Marcel wins even with another file
        manager installed. The NixOS module only installs the claiming
        variant; with a second file manager on the system, which one D-Bus
        starts is then decided by profile order, so prefer the Home Manager
        module when that matters.
      '';
    };

    settings = {
      theme = lib.mkOption {
        type = lib.types.enum [
          "nord"
          "gruvbox-dark"
          "tokyo-night"
          "catppuccin-mocha"
          "dracula"
          "one-dark"
          "solarized-dark"
          "everforest-dark"
          "rose-pine"
          "kanagawa-wave"
          "system-dark"
          "system-light"
        ];
        default = "nord";
        description = "Initial Marcel color palette.";
      };

      icon_theme = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "Breeze";
        description = ''
          Explicit freedesktop icon-theme override. Null keeps Marcel's
          bundled Nordzy icons ahead of the ambient GTK theme.
        '';
      };

      ui_font = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "IBM Plex Mono";
        description = ''
          Exact installed font-family name for both UI and monospace roles.
          Null uses Marcel's bundled Iosevka Mono family.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      (
        if isHome then
          { home.packages = [ configuredPackage ]; }
        else
          { environment.systemPackages = [ configuredPackage ]; }
      )

      (lib.mkIf cfg.defaultDirectoryHandler (
        if isHome then
          {
            xdg.mimeApps.enable = true;
            xdg.mimeApps.defaultApplications."inode/directory" = [ "io.github.berker_z.Marcel.desktop" ];
          }
        else
          {
            xdg.mime.enable = true;
            xdg.mime.defaultApplications."inode/directory" = "io.github.berker_z.Marcel.desktop";
          }
      ))

      (lib.mkIf (cfg.fileManager1 && isHome) {
        xdg.dataFile."dbus-1/services/org.freedesktop.FileManager1.service".text = fileManager1Service;
      })
    ]
  );
}
