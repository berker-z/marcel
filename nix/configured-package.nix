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
    wrapProgram "$out/bin/marcel-rs" ${lib.escapeShellArgs wrapperArgs}

    # D-Bus activation must launch this wrapper, not the binary underneath,
    # or a Marcel started by "show in folder" runs without these settings.
    # The generic file exists only when wrapping the file-manager1 variant.
    for name in io.github.berker_z.Marcel org.freedesktop.FileManager1; do
      service="$out/share/dbus-1/services/$name.service"
      if [[ -e "$service" ]]; then
        cp --remove-destination \
          "${marcel}/share/dbus-1/services/$name.service" \
          "$service"
        substituteInPlace "$service" \
          --replace-fail "${marcel}/bin/marcel-rs" "$out/bin/marcel-rs"
      fi
    done
  '';

  inherit (marcel) meta;
  passthru = {
    unconfigured = marcel;
    inherit settings;
  };
}
