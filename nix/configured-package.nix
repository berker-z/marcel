{
  lib,
  symlinkJoin,
  makeWrapper,
  marcel,
  settings ? { },
}:
let
  resolved = {
    theme = "nord";
    icon_theme = null;
    ui_font = null;
  }
  // settings;
  wrapperArgs = [
    "--set"
    "MARCEL_THEME"
    resolved.theme
  ]
  ++ lib.optionals (resolved.icon_theme != null) [
    "--set"
    "MARCEL_ICON_THEME"
    resolved.icon_theme
  ]
  ++ lib.optionals (resolved.ui_font != null) [
    "--set"
    "MARCEL_FONT_FAMILY"
    resolved.ui_font
  ];
in
symlinkJoin {
  name = "marcel-configured-${marcel.version or "unknown"}";
  paths = [ marcel ];
  nativeBuildInputs = [ makeWrapper ];

  postBuild = ''
    wrapProgram "$out/bin/marcel" ${lib.escapeShellArgs wrapperArgs}

    service="$out/share/dbus-1/services/io.github.berker_z.Marcel.service"
    if [[ -e "$service" ]]; then
      cp --remove-destination \
        "${marcel}/share/dbus-1/services/io.github.berker_z.Marcel.service" \
        "$service"
      substituteInPlace "$service" \
        --replace-fail "${marcel}/bin/marcel" "$out/bin/marcel"
    fi
  '';

  inherit (marcel) meta;
  passthru = {
    unconfigured = marcel;
    inherit settings;
  };
}
