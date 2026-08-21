{
  flake,
  packageOption,
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
  configuredPackage = pkgs.callPackage ./configured-package.nix {
    marcel = cfg.package;
    inherit (cfg) settings;
  };
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

  config = lib.mkIf cfg.enable (lib.setAttrByPath packageOption [ configuredPackage ]);
}
