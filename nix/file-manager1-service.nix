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

    branded_service="$out/share/dbus-1/services/io.github.berker_z.Marcel.service"
    cp --remove-destination \
      "${marcel}/share/dbus-1/services/io.github.berker_z.Marcel.service" \
      "$branded_service"
    substituteInPlace "$branded_service" \
      --replace-fail "${marcel}/bin/marcel-rs" "$out/bin/marcel-rs"

    install -Dm644 ${./org.freedesktop.FileManager1.service} \
      "$out/share/dbus-1/services/org.freedesktop.FileManager1.service"
    substituteInPlace \
      "$out/share/dbus-1/services/org.freedesktop.FileManager1.service" \
      --replace-fail @marcel@ "$out"
  '';

  inherit (marcel) meta;
  passthru.unconfigured = marcel;
}
