{
  symlinkJoin,
  makeWrapper,
  marcel,
}:
symlinkJoin {
  name = "marcel-file-manager1-service";
  paths = [ marcel ];
  nativeBuildInputs = [ makeWrapper ];

  postBuild = ''
    wrapProgram "$out/bin/marcel-rs" --set MARCEL_CLAIM_FILE_MANAGER1 1

    # Every activation file underneath must start this wrapper. This may sit
    # over the file-chooser variant, so re-point whatever service files are
    # there rather than a fixed list.
    for service in "$out/share/dbus-1/services/"*.service; do
      cp --remove-destination "$(readlink -f "$service")" "$service"
      substituteInPlace "$service" \
        --replace-fail "${marcel}/bin/marcel-rs" "$out/bin/marcel-rs"
    done

    install -Dm644 ${./org.freedesktop.FileManager1.service} \
      "$out/share/dbus-1/services/org.freedesktop.FileManager1.service"
    substituteInPlace \
      "$out/share/dbus-1/services/org.freedesktop.FileManager1.service" \
      --replace-fail @marcel@ "$out"
  '';

  inherit (marcel) meta version;
  passthru.unconfigured = marcel;
}
