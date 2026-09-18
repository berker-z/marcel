{
  lib,
  symlinkJoin,
  makeWrapper,
  ffmpeg-headless,
  marcel,
  settings ? { },
}:
let
  resolved = {
    theme = "nord";
    icon_theme = null;
    ui_font = null;
    media = false;
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
  ]
  # ffmpeg is found on PATH rather than bundled: it is about 300 MiB of
  # closure against Marcel's 224, and only video previews and Opus audio
  # need it. `settings.media` puts it on the wrapper's PATH for people who
  # want those guaranteed rather than dependent on what else is installed.
  ++ lib.optionals resolved.media [
    "--prefix"
    "PATH"
    ":"
    (lib.makeBinPath [ ffmpeg-headless ])
  ];
in
symlinkJoin {
  name = "marcel-configured-${marcel.version or "unknown"}";
  paths = [ marcel ];
  nativeBuildInputs = [ makeWrapper ];

  postBuild = ''
    wrapProgram "$out/bin/marcel-rs" ${lib.escapeShellArgs wrapperArgs}

    # D-Bus activation must launch this wrapper, not the binary underneath,
    # or a Marcel started by "show in folder" or by a file dialog runs
    # without these settings. Which service files exist depends on the
    # variants wrapped below, so re-point whatever is there.
    for service in "$out/share/dbus-1/services/"*.service; do
      cp --remove-destination "$(readlink -f "$service")" "$service"
      substituteInPlace "$service" \
        --replace-fail "${marcel}/bin/marcel-rs" "$out/bin/marcel-rs"
    done
  '';

  inherit (marcel) meta;
  passthru = {
    unconfigured = marcel;
    inherit settings;
  };
}
